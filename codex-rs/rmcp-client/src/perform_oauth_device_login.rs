use std::collections::HashMap;
use std::fmt;
use std::future::Future;
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
use base64::Engine;
use oauth2::PkceCodeChallenge;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::auth::AuthorizationMetadata;
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
    // RMCP requires resolved metadata to be installed before client configuration.
    auth_manager.set_metadata(metadata.clone());

    let credentials = OAuthClientCredentials::resolve(oauth_config)?;
    let client_id = match credentials.client_id {
        Some(client_id) => client_id,
        None => resolve_device_client_id(
            &oauth_http_client, &metadata, scopes, client_registration,
        ).await?,
    };
    let client_auth_method = device_client_auth_method(&metadata, credentials.client_secret.is_some());
    let mut client_config = OAuthClientConfig::new(&client_id, server_url)
        .with_scopes(scopes.to_vec());
    if let Some(secret) = credentials.client_secret.as_ref() {
        client_config = client_config.with_client_secret(secret.secret());
    }
    auth_manager.configure_client(client_config)?;

    let device_endpoint = device_authorization_endpoint(&metadata)?;
    validate_oauth_endpoint(&device_endpoint)?;
    let token_endpoint = metadata.token_endpoint.clone();
    validate_oauth_endpoint(&token_endpoint)?;

    let (details, pkce) = request_device_authorization_with_pkce_fallback(
        &oauth_http_client, &device_endpoint, &client_id,
        credentials.client_secret.as_ref().map(|secret| secret.secret().as_str()),
        client_auth_method, scopes, oauth_resource,
    ).await?;
    show_authorization_prompt(DeviceAuthorizationPrompt::new(server_name, &details));
    let token_response = match poll_device_token(
        &oauth_http_client, &token_endpoint, &client_id,
        credentials.client_secret.as_ref().map(|secret| secret.secret().as_str()),
        client_auth_method, oauth_resource, &details, pkce.as_ref(),
    ).await {
        Ok(tokens) => tokens,
        Err(error) if pkce.is_some() && is_invalid_request_provider_error(&error) => {
            let details = request_device_authorization(
                &oauth_http_client, &device_endpoint, &client_id,
                credentials.client_secret.as_ref().map(|secret| secret.secret().as_str()),
                client_auth_method, scopes, oauth_resource, None, None,
            ).await.context("OAuth device token endpoint rejected PKCE verifier, and retry without PKCE failed during device authorization")?;
            show_authorization_prompt(DeviceAuthorizationPrompt::new(server_name, &details));
            poll_device_token(
                &oauth_http_client, &token_endpoint, &client_id,
                credentials.client_secret.as_ref().map(|secret| secret.secret().as_str()),
                client_auth_method, oauth_resource, &details, None,
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
    metadata: &AuthorizationMetadata,
    scopes: &[String],
    registration: McpOAuthClientRegistration,
) -> Result<String> {
    if registration == McpOAuthClientRegistration::Cimd {
        bail!("MCP device authorization requires a configured public client ID or Dynamic Client Registration; CIMD is not supported for this device flow");
    }
    let endpoint = dynamic_registration_endpoint(metadata)?;
    validate_oauth_endpoint(&endpoint)?;
    let supports_refresh = metadata.additional_fields.get("grant_types_supported")
        .and_then(serde_json::Value::as_array)
        .is_none_or(|grants| grants.iter().any(|grant| grant.as_str() == Some("refresh_token")));
    let mut grant_types = vec![DEVICE_CODE_GRANT_TYPE];
    if supports_refresh { grant_types.push("refresh_token"); }
    let request = DeviceClientRegistrationRequest {
        client_name: "Codex", grant_types, token_endpoint_auth_method: "none",
        scope: (!scopes.is_empty()).then(|| scopes.join(" ")),
    };
    let response = oauth_post(http, &endpoint, "application/json", serde_json::to_vec(&request)?, None, None).await?;
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
fn dynamic_registration_endpoint(metadata: &AuthorizationMetadata) -> Result<String> {
    metadata.registration_endpoint.clone()
        .filter(|endpoint| !endpoint.trim().is_empty())
        .ok_or_else(|| anyhow!("OAuth device login requires a configured public client ID because dynamic client registration is not advertised"))
}

fn device_authorization_endpoint(metadata: &AuthorizationMetadata) -> Result<String> {
    metadata_endpoint(&metadata.additional_fields, "device_authorization_endpoint")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeviceClientAuthMethod { None, ClientSecretBasic, ClientSecretPost }

fn device_client_auth_method(metadata: &AuthorizationMetadata, has_secret: bool) -> DeviceClientAuthMethod {
    if !has_secret { return DeviceClientAuthMethod::None; }
    let methods = metadata.additional_fields.get("token_endpoint_auth_methods_supported")
        .and_then(serde_json::Value::as_array);
    let Some(methods) = methods else { return DeviceClientAuthMethod::ClientSecretBasic; };
    let has_basic = methods.iter().any(|method| method.as_str() == Some("client_secret_basic"));
    let has_post = methods.iter().any(|method| method.as_str() == Some("client_secret_post"));
    if has_basic { DeviceClientAuthMethod::ClientSecretBasic }
    else if has_post { DeviceClientAuthMethod::ClientSecretPost }
    else { DeviceClientAuthMethod::None }
}

fn basic_credentials<'a>(method: DeviceClientAuthMethod, client_id: &'a str, secret: Option<&'a str>) -> Option<(&'a str, &'a str)> {
    if method != DeviceClientAuthMethod::ClientSecretBasic { return None; }
    secret.map(|secret| (client_id, secret))
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
    client_secret: Option<&str>, auth_method: DeviceClientAuthMethod, scopes: &[String], resource: Option<&str>,
) -> Result<(DeviceAuthorizationResponse, Option<DevicePkce>)> {
    let pkce = DevicePkce::new_random();
    match request_device_authorization(
        http, endpoint, client_id, client_secret, auth_method, scopes, resource, Some(&pkce), None,
    ).await {
        Ok(details) => Ok((details, Some(pkce))),
        Err(error) if is_invalid_request_provider_error(&error) => {
            let details = request_device_authorization(
                http, endpoint, client_id, client_secret, auth_method, scopes, resource, None, None,
            ).await.context("OAuth device authorization rejected PKCE parameters, and retry without PKCE failed")?;
            Ok((details, None))
        }
        Err(error) => Err(error),
    }
}

async fn request_device_authorization(
    http: &OAuthHttpClientAdapter, endpoint: &str, client_id: &str,
    client_secret: Option<&str>, auth_method: DeviceClientAuthMethod, scopes: &[String], resource: Option<&str>,
    pkce: Option<&DevicePkce>, deadline: Option<Instant>,
) -> Result<DeviceAuthorizationResponse> {
    let fields = device_authorization_fields(client_id, client_secret, auth_method, scopes, resource, pkce);
    let response = oauth_post(
        http, endpoint, "application/x-www-form-urlencoded", form_body(&fields), deadline,
        basic_credentials(auth_method, client_id, client_secret),
    ).await?;
    if !response.status().is_success() {
        return Err(provider_error_from_body(response.status(), response.body(), "device authorization"));
    }
    let details = serde_json::from_slice::<DeviceAuthorizationResponse>(response.body())
        .context("failed to parse OAuth device authorization response")?;
    validate_verification_uri(details.verification_uri_complete.as_deref().unwrap_or(&details.verification_uri))?;
    Ok(details)
}

fn device_authorization_fields(
    client_id: &str, client_secret: Option<&str>, auth_method: DeviceClientAuthMethod,
    scopes: &[String], resource: Option<&str>, pkce: Option<&DevicePkce>,
) -> Vec<(&'static str, String)> {
    let mut fields = vec![("client_id", client_id.to_string())];
    if let Some(secret) = form_client_secret(auth_method, client_secret) {
        fields.push(("client_secret", secret.to_string()));
    }
    if !scopes.is_empty() { fields.push(("scope", scopes.join(" "))); }
    if let Some(resource) = resource.filter(|resource| !resource.trim().is_empty()) {
        fields.push(("resource", resource.to_string()));
    }
    if let Some(pkce) = pkce {
        fields.push(("code_challenge", pkce.code_challenge.clone()));
        fields.push(("code_challenge_method", "S256".to_string()));
    }
    fields
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
    auth_method: DeviceClientAuthMethod, resource: Option<&str>, details: &DeviceAuthorizationResponse, pkce: Option<&DevicePkce>,
) -> Result<OAuthTokenResponse> {
    let deadline = Instant::now() + Duration::from_secs(device_authorization_expires_in(details));
    let mut interval = Duration::from_secs(details.interval.unwrap_or(DEFAULT_DEVICE_POLL_INTERVAL_SECS).max(1));
    loop {
        let fields = token_form(client_id, client_secret, auth_method, resource, details, pkce);
        let response = oauth_post(
            http, endpoint, "application/x-www-form-urlencoded", form_body(&fields), Some(deadline),
            basic_credentials(auth_method, client_id, client_secret),
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
    client_id: &str, client_secret: Option<&str>, auth_method: DeviceClientAuthMethod, resource: Option<&str>,
    details: &DeviceAuthorizationResponse, pkce: Option<&DevicePkce>,
) -> Vec<(&'static str, String)> {
    let mut fields = vec![
        ("grant_type", DEVICE_CODE_GRANT_TYPE.to_string()),
        ("device_code", details.device_code.clone()),
        ("client_id", client_id.to_string()),
    ];
    if let Some(secret) = form_client_secret(auth_method, client_secret) {
        fields.push(("client_secret", secret.to_string()));
    }
    if let Some(resource) = resource.filter(|resource| !resource.trim().is_empty()) {
        fields.push(("resource", resource.to_string()));
    }
    if let Some(pkce) = pkce { fields.push(("code_verifier", pkce.code_verifier.clone())); }
    fields
}
fn form_client_secret<'a>(method: DeviceClientAuthMethod, secret: Option<&'a str>) -> Option<&'a str> {
    (method == DeviceClientAuthMethod::ClientSecretPost).then_some(secret).flatten()
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
    deadline: Option<Instant>, basic_credentials: Option<(&str, &str)>,
) -> Result<http::Response<Vec<u8>>> {
    let timeout = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()))
        .unwrap_or(HTTP_OPERATION_TIMEOUT);
    if timeout.is_zero() { return Err(device_expired_error()); }
    let request = build_oauth_request(endpoint, content_type, body, basic_credentials)?;
    let operation = async {
        http.execute_request(request, OAuthHttpRedirectPolicy::Stop, Some(timeout))
            .await
            .map_err(|error| anyhow!("OAuth device request failed: {error}"))
    };
    match deadline {
        Some(deadline) => await_device_deadline(deadline, operation).await,
        None => tokio::time::timeout(HTTP_OPERATION_TIMEOUT, operation)
            .await
            .map_err(|_| anyhow!("OAuth device request timed out"))?,
    }
}
fn build_oauth_request(
    endpoint: &str, content_type: &str, body: Vec<u8>, basic_credentials: Option<(&str, &str)>,
) -> Result<http::Request<Vec<u8>>> {
    let mut builder = http::Request::builder()
        .method(http::Method::POST).uri(endpoint)
        .header(http::header::CONTENT_TYPE, content_type);
    if let Some((client_id, secret)) = basic_credentials {
        let encoded_client_id: String = url::form_urlencoded::byte_serialize(client_id.as_bytes()).collect();
        let encoded_secret: String = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{encoded_client_id}:{encoded_secret}"));
        builder = builder.header(http::header::AUTHORIZATION, format!("Basic {encoded}"));
    }
    builder.body(body).context("failed to build OAuth device request")
}

async fn await_device_deadline<T>(
    deadline: Instant,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(device_expired_error());
    }
    tokio::time::timeout(remaining, operation)
        .await
        .map_err(|_| device_expired_error())?
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
    use futures::future::BoxFuture;
    use codex_exec_server::HttpClient;
    use codex_exec_server::HttpRequestParams;
    use codex_exec_server::HttpRequestResponse;
    use codex_exec_server::HttpResponseBodyStream;
    use codex_exec_server::ExecServerError;
    use codex_exec_server::HttpHeader;
    use codex_exec_server::ByteChunk;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn client_secret_auth_methods_are_applied_without_secret_leaks() {
        let basic_body = form_body(&[("client_id", "public-id".to_string())]);
        let basic = build_oauth_request("https://issuer.example/device", "application/x-www-form-urlencoded", basic_body, Some(("id +:%", "secret +:%"))).expect("basic request");
        let expected_credentials = base64::engine::general_purpose::STANDARD.encode("id+%2B%3A%25:secret+%2B%3A%25");
        assert_eq!(basic.headers()[http::header::AUTHORIZATION], format!("Basic {expected_credentials}"));
        assert!(!String::from_utf8(basic.body().clone()).expect("body").contains("secret +:%"));

        let post_body = form_body(&[("client_id", "public-id".to_string()), ("client_secret", "never-log-this".to_string())]);
        let post = build_oauth_request("https://issuer.example/token", "application/x-www-form-urlencoded", post_body, None).expect("post request");
        assert!(post.headers().get(http::header::AUTHORIZATION).is_none());
        assert!(String::from_utf8(post.body().clone()).expect("body").contains("client_secret=never-log-this"));
        assert_eq!(form_client_secret(DeviceClientAuthMethod::ClientSecretPost, Some("never-log-this")), Some("never-log-this"));
        assert_eq!(form_client_secret(DeviceClientAuthMethod::ClientSecretBasic, Some("never-log-this")), None);
    }

    #[test]
    fn metadata_selected_client_auth_method_reaches_both_device_and_token_forms() {
        let basic_metadata: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
            "authorization_endpoint":"https://issuer.example/authorize",
            "token_endpoint":"https://issuer.example/token",
            "token_endpoint_auth_methods_supported":["client_secret_basic","client_secret_post"]
        })).expect("metadata");
        let basic_method = device_client_auth_method(&basic_metadata, true);
        assert_eq!(basic_method, DeviceClientAuthMethod::ClientSecretBasic);
        let scope = vec!["ops:read".to_string()];
        let details = DeviceAuthorizationResponse { device_code:"device".into(), user_code:"user".into(), verification_uri:"https://issuer.example/verify".into(), verification_uri_complete:None, expires_in:Some(60), interval:Some(1) };
        let device = device_authorization_fields("client", Some("secret"), basic_method, &scope, None, None);
        let token = token_form("client", Some("secret"), basic_method, None, &details, None);
        assert!(!device.iter().any(|(key, _)| *key == "client_secret"));
        assert!(!token.iter().any(|(key, _)| *key == "client_secret"));
        assert!(basic_credentials(basic_method, "client", Some("secret")).is_some());

        let post_metadata: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
            "authorization_endpoint":"https://issuer.example/authorize",
            "token_endpoint":"https://issuer.example/token",
            "token_endpoint_auth_methods_supported":["client_secret_post"]
        })).expect("metadata");
        let post_method = device_client_auth_method(&post_metadata, true);
        assert_eq!(post_method, DeviceClientAuthMethod::ClientSecretPost);
        for secret_field in [
            device_authorization_fields("client", Some("secret"), post_method, &scope, None, None).iter().any(|(key, _)| *key == "client_secret"),
            token_form("client", Some("secret"), post_method, None, &details, None).iter().any(|(key, _)| *key == "client_secret"),
        ] { assert!(secret_field); }
        assert!(basic_credentials(post_method, "client", Some("secret")).is_none());
    }

    struct ScriptedHttpClient {
        requests: Mutex<Vec<HttpRequestParams>>,
        responses: Mutex<VecDeque<(u16, Vec<u8>)>>,
    }
    impl ScriptedHttpClient {
        fn new(responses: impl IntoIterator<Item = (u16, Vec<u8>)>) -> Self {
            Self { requests: Mutex::new(Vec::new()), responses: Mutex::new(responses.into_iter().collect()) }
        }

        fn next_response(&self, params: HttpRequestParams) -> (HttpRequestResponse, Vec<u8>) {
            self.requests.lock().expect("request lock").push(params);
            let (status, body) = self.responses.lock().expect("response lock").pop_front().expect("scripted response");
            let response = HttpRequestResponse {
                status,
                headers: vec![HttpHeader { name: "content-type".into(), value: "application/json".into(), value_env_var: None }],
                body: ByteChunk::from(Vec::new()),
            };
            (response, body)
        }
    }
    impl HttpClient for ScriptedHttpClient {
        fn http_request(
            &self,
            params: HttpRequestParams,
        ) -> BoxFuture<'_, std::result::Result<HttpRequestResponse, ExecServerError>> {
            let (response, body) = self.next_response(params);
            let response = HttpRequestResponse { body: ByteChunk::from(body), ..response };
            Box::pin(async move { Ok(response) })
        }

        fn http_request_stream(
            &self,
            params: HttpRequestParams,
        ) -> BoxFuture<'_, std::result::Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
            let (response, body) = self.next_response(params);
            let stream = HttpResponseBodyStream::from_chunks(vec![body]);
            Box::pin(async move { Ok((response, stream)) })
        }
    }

    fn test_oauth_http_adapter(client: Arc<dyn HttpClient>) -> OAuthHttpClientAdapter {
        OAuthHttpClientAdapter::new_with_redirect_mode(
            client, http::HeaderMap::new(), "https://issuer.example", false,
            StreamableHttpRedirectMode::Legacy,
        ).expect("OAuth HTTP adapter")
    }

    fn request_form(params: &HttpRequestParams) -> Vec<(String, String)> {
        url::form_urlencoded::parse(params.body.as_ref().expect("request body").as_ref())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    #[tokio::test]
    async fn client_auth_methods_are_applied_to_actual_device_and_token_requests() {
        let device_response = br#"{"device_code":"device","user_code":"user","verification_uri":"https://issuer.example/verify","expires_in":60}"#.to_vec();
        let token_response = br#"{"access_token":"token","token_type":"Bearer","expires_in":3600}"#.to_vec();
        for (methods, expected_method) in [
            (vec!["client_secret_basic", "client_secret_post"], DeviceClientAuthMethod::ClientSecretBasic),
            (vec!["client_secret_post"], DeviceClientAuthMethod::ClientSecretPost),
        ] {
            let metadata: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
                "authorization_endpoint":"https://issuer.example/authorize",
                "token_endpoint":"https://issuer.example/token",
                "token_endpoint_auth_methods_supported":methods
            })).expect("metadata");
            let method = device_client_auth_method(&metadata, true);
            assert_eq!(method, expected_method);
            let client = Arc::new(ScriptedHttpClient::new([
                (200, device_response.clone()),
                (200, token_response.clone()),
            ]));
            let adapter = test_oauth_http_adapter(client.clone());
            let scopes = ["ops:read".to_string()];
            let details = request_device_authorization(
                &adapter, "https://issuer.example/device", "client: +",
                Some("secret +:%"), method, &scopes, None, None, None,
            ).await.expect("device authorization");
            poll_device_token(
                &adapter, "https://issuer.example/token", "client: +",
                Some("secret +:%"), method, None, &details, None,
            ).await.expect("device token");
            let requests = client.requests.lock().expect("request lock");
            assert_eq!(requests.len(), 2);
            for request in requests.iter() {
                let authorization = request.headers.iter().find(|header| header.name.eq_ignore_ascii_case("authorization"));
                match expected_method {
                    DeviceClientAuthMethod::ClientSecretBasic => {
                        let expected = base64::engine::general_purpose::STANDARD.encode("client%3A+%2B:secret+%2B%3A%25");
                        let expected = format!("Basic {expected}");
                        assert_eq!(authorization.map(|header| header.value.as_str()), Some(expected.as_str()));
                        assert!(!request_form(request).iter().any(|(key, _)| key == "client_secret"));
                    }
                    DeviceClientAuthMethod::ClientSecretPost => {
                        assert!(authorization.is_none());
                        assert!(request_form(request).iter().any(|(key, value)| key == "client_secret" && value == "secret +:%"));
                    }
                    DeviceClientAuthMethod::None => unreachable!("configured secret selected a secret auth method"),
                }
            }
        }
    }

    #[tokio::test]
    async fn pkce_fallback_retries_actual_device_requests_with_resolved_scopes() {
        let success = br#"{"device_code":"device","user_code":"user","verification_uri":"https://issuer.example/verify","expires_in":60}"#.to_vec();
        let client = Arc::new(ScriptedHttpClient::new([
            (400, br#"{"error":"invalid_request"}"#.to_vec()),
            (200, success),
        ]));
        let adapter = test_oauth_http_adapter(client.clone());
        let scopes = ["ops:read".to_string(), "ops:write".to_string()];
        let (details, pkce) = request_device_authorization_with_pkce_fallback(
            &adapter, "https://issuer.example/device", "client", None,
            DeviceClientAuthMethod::None, &scopes, None,
        ).await.expect("fallback request");
        assert_eq!(details.device_code, "device");
        assert!(pkce.is_none());
        let requests = client.requests.lock().expect("request lock");
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            let fields = request_form(request);
            assert!(fields.iter().any(|(key, value)| key == "scope" && value == "ops:read ops:write"));
        }
        assert!(request_form(&requests[0]).iter().any(|(key, _)| key == "code_challenge"));
        assert!(!request_form(&requests[1]).iter().any(|(key, _)| key == "code_challenge"));
    }

    #[tokio::test]
    async fn typed_registration_metadata_causes_registration_request() {
        let metadata: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
            "authorization_endpoint":"https://issuer.example/authorize",
            "token_endpoint":"https://issuer.example/token",
            "registration_endpoint":"https://issuer.example/register",
            "device_authorization_endpoint":"https://issuer.example/device",
            "grant_types_supported":["urn:ietf:params:oauth:grant-type:device_code"]
        })).expect("metadata");
        let response = br#"{"client_id":"dynamic-client"}"#.to_vec();
        let client = Arc::new(ScriptedHttpClient::new([(201, response)]));
        let adapter = test_oauth_http_adapter(client.clone());
        let client_id = resolve_device_client_id(
            &adapter, &metadata, &["ops:read".to_string()], McpOAuthClientRegistration::Auto,
        ).await.expect("dynamic registration");
        assert_eq!(client_id, "dynamic-client");
        let requests = client.requests.lock().expect("request lock");
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].url, "https://issuer.example/register");
    }

    #[tokio::test]
    async fn configured_client_can_be_configured_from_resolved_metadata() {
        let metadata: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
            "authorization_endpoint":"https://issuer.example/authorize",
            "token_endpoint":"https://issuer.example/token",
            "device_authorization_endpoint":"https://issuer.example/device"
        })).expect("metadata");
        let endpoint = device_authorization_endpoint(&metadata).expect("device endpoint");
        let mut manager = AuthorizationManager::new("https://mcp.example")
            .await.expect("authorization manager");
        manager.set_metadata(metadata);
        manager.configure_client(OAuthClientConfig::new("client", "https://mcp.example"))
            .expect("configured client with installed metadata");
        assert_eq!(endpoint, "https://issuer.example/device");
    }

    #[test]
    fn typed_metadata_registration_endpoint_is_used() {
        let metadata: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
            "authorization_endpoint":"https://issuer.example/authorize",
            "token_endpoint":"https://issuer.example/token",
            "registration_endpoint":"https://issuer.example/register",
            "device_authorization_endpoint":"https://issuer.example/device"
        })).expect("metadata");
        assert_eq!(dynamic_registration_endpoint(&metadata).expect("registration endpoint"), "https://issuer.example/register");
        assert_eq!(device_authorization_endpoint(&metadata).expect("device endpoint"), "https://issuer.example/device");
    }

    struct DropMarker(Arc<AtomicUsize>);
    impl Drop for DropMarker {
        fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    struct PendingHttpClient {
        dropped: Arc<AtomicUsize>,
    }
    impl HttpClient for PendingHttpClient {
        fn http_request(
            &self,
            _params: HttpRequestParams,
        ) -> BoxFuture<'_, std::result::Result<HttpRequestResponse, ExecServerError>> {
            let marker = DropMarker(Arc::clone(&self.dropped));
            Box::pin(async move {
                let _marker = marker;
                std::future::pending().await
            })
        }

        fn http_request_stream(
            &self,
            _params: HttpRequestParams,
        ) -> BoxFuture<'_, std::result::Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
            let marker = DropMarker(Arc::clone(&self.dropped));
            Box::pin(async move {
                let _marker = marker;
                std::future::pending().await
            })
        }
    }

    #[tokio::test]
    async fn in_flight_device_token_http_request_is_cancelled_at_expiry() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let http_client: Arc<dyn HttpClient> = Arc::new(PendingHttpClient { dropped: Arc::clone(&dropped) });
        let http = OAuthHttpClientAdapter::new_with_redirect_mode(
            http_client, http::HeaderMap::new(), "https://issuer.example", false,
            StreamableHttpRedirectMode::Legacy,
        ).expect("OAuth HTTP adapter");
        let deadline = Instant::now() + Duration::from_millis(10);
        let error = oauth_post(
            &http, "https://issuer.example/token", "application/x-www-form-urlencoded",
            b"grant_type=device_code".to_vec(), Some(deadline), None,
        ).await.expect_err("request must expire");
        assert!(error.to_string().contains("expired"));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}
