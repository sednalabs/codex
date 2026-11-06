use crate::oauth_metadata::discover_metadata;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::HttpClient;
use codex_protocol::protocol::McpAuthStatus;
use futures::FutureExt;
use reqwest::Client;
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::header::AUTHORIZATION;
use reqwest::header::HeaderMap;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::auth::AuthError;
use rmcp::transport::auth::AuthorizationMetadata;
use serde::Deserialize;
use tracing::debug;

use crate::oauth::StoredOAuthTokenStatus;
use crate::oauth::oauth_token_status;
use crate::oauth_http_client::OAuthHttpClientAdapter;
use crate::utils::apply_default_headers;
use crate::utils::build_default_headers;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const OAUTH_DISCOVERY_HEADER: &str = "MCP-Protocol-Version";
const OAUTH_DISCOVERY_VERSION: &str = "2024-11-05";

/// Timeout policy for OAuth metadata discovery through a supplied HTTP client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthDiscoveryTimeout {
    /// Preserve the timeout requested by the OAuth implementation.
    Requested,
    /// Cap OAuth discovery requests at the supplied duration.
    Capped(Duration),
}

impl OAuthDiscoveryTimeout {
    /// Preserves the existing timeout for local OAuth discovery.
    pub const LOCAL: Self = Self::Capped(DISCOVERY_TIMEOUT);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamableHttpOAuthDiscovery {
    pub issuer: Option<String>,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: String,
    pub scopes_supported: Option<Vec<String>>,
    pub device_authorization_endpoint: Option<String>,
    pub grant_types_supported: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpLoginRequirement {
    Login,
    Reauthentication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthState {
    Unsupported,
    LoggedOut(McpLoginRequirement),
    BearerToken,
    OAuth,
}

impl From<McpAuthState> for McpAuthStatus {
    fn from(value: McpAuthState) -> Self {
        match value {
            McpAuthState::Unsupported => Self::Unsupported,
            McpAuthState::LoggedOut(_) => Self::NotLoggedIn,
            McpAuthState::BearerToken => Self::BearerToken,
            McpAuthState::OAuth => Self::OAuth,
        }
    }
}

enum AuthStatusCheck {
    Complete(McpAuthState),
    Discover(HeaderMap),
}

/// Determine the authentication status for a streamable HTTP MCP server.
pub async fn determine_streamable_http_auth_status(
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<McpAuthState> {
    let default_headers = match auth_status_before_discovery(
        server_name,
        url,
        bearer_token_env_var,
        http_headers,
        env_http_headers,
        store_mode,
        keyring_backend_kind,
    )? {
        AuthStatusCheck::Complete(status) => return Ok(status),
        AuthStatusCheck::Discover(default_headers) => default_headers,
    };

    determine_auth_status_from_discovery(
        server_name,
        url,
        discover_streamable_http_oauth_with_headers(url, &default_headers).await,
    )
}

/// Determine authentication status while routing OAuth discovery through the
/// provided HTTP client.
#[allow(clippy::too_many_arguments)]
pub async fn determine_streamable_http_auth_status_with_http_client(
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_client: Arc<dyn HttpClient>,
    discovery_timeout: OAuthDiscoveryTimeout,
) -> Result<McpAuthState> {
    let default_headers = match auth_status_before_discovery(
        server_name,
        url,
        bearer_token_env_var,
        http_headers,
        env_http_headers,
        store_mode,
        keyring_backend_kind,
    )? {
        AuthStatusCheck::Complete(status) => return Ok(status),
        AuthStatusCheck::Discover(default_headers) => default_headers,
    };
    determine_auth_status_from_discovery(
        server_name,
        url,
        discover_streamable_http_oauth_with_headers_and_http_client(
            url,
            default_headers,
            http_client,
            discovery_timeout,
        )
        .await,
    )
}

/// Determine authentication status using only configured and stored credentials.
///
/// Returns `None` when determining the status would require OAuth metadata discovery.
pub fn determine_streamable_http_auth_status_from_credentials(
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<Option<McpAuthState>> {
    match auth_status_before_discovery(
        server_name,
        url,
        bearer_token_env_var,
        http_headers,
        env_http_headers,
        store_mode,
        keyring_backend_kind,
    )? {
        AuthStatusCheck::Complete(status) => Ok(Some(status)),
        AuthStatusCheck::Discover(_) => Ok(None),
    }
}

fn auth_status_before_discovery(
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
) -> Result<AuthStatusCheck> {
    if bearer_token_env_var.is_some() {
        return Ok(AuthStatusCheck::Complete(McpAuthState::BearerToken));
    }

    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    if default_headers.contains_key(AUTHORIZATION) {
        return Ok(AuthStatusCheck::Complete(McpAuthState::BearerToken));
    }

    match oauth_token_status(server_name, url, store_mode, keyring_backend_kind)? {
        StoredOAuthTokenStatus::Usable => {
            return Ok(AuthStatusCheck::Complete(McpAuthState::OAuth));
        }
        StoredOAuthTokenStatus::AuthorizationRequired => {
            return Ok(AuthStatusCheck::Complete(McpAuthState::LoggedOut(
                McpLoginRequirement::Reauthentication,
            )));
        }
        StoredOAuthTokenStatus::Missing => {}
    }

    Ok(AuthStatusCheck::Discover(default_headers))
}

fn determine_auth_status_from_discovery(
    server_name: &str,
    url: &str,
    discovery: Result<Option<StreamableHttpOAuthDiscovery>>,
) -> Result<McpAuthState> {
    match discovery {
        Ok(Some(_)) => Ok(McpAuthState::LoggedOut(McpLoginRequirement::Login)),
        Ok(None) => Ok(McpAuthState::Unsupported),
        Err(error) => {
            debug!(
                "failed to detect OAuth support for MCP server `{server_name}` at {url}: {error:?}"
            );
            Ok(McpAuthState::Unsupported)
        }
    }
}

/// Attempt to determine whether a streamable HTTP MCP server advertises OAuth login.
pub async fn supports_oauth_login(url: &str) -> Result<bool> {
    Ok(discover_streamable_http_oauth(
        url, /*http_headers*/ None, /*env_http_headers*/ None,
    )
    .await?
    .is_some())
}

pub async fn discover_streamable_http_oauth(
    url: &str,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    discover_streamable_http_oauth_with_headers(url, &default_headers).await
}

pub async fn discover_streamable_http_oauth_with_http_client(
    url: &str,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    http_client: Arc<dyn HttpClient>,
    discovery_timeout: OAuthDiscoveryTimeout,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    discover_streamable_http_oauth_with_headers_and_http_client(
        url,
        default_headers,
        http_client,
        discovery_timeout,
    )
    .await
}

pub(crate) async fn discover_streamable_http_oauth_with_headers(
    url: &str,
    default_headers: &HeaderMap,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    // Preserve the direct, no-proxy route while scoping resource headers to their origin.
    let client = codex_http_client::HttpClientBuilder::new()
        .without_redirects()
        .without_retries()
        .without_request_logging()
        .build_direct()?;
    let oauth_http_client = OAuthHttpClientAdapter::new_direct_scoped(
        client,
        default_headers.clone(),
        url,
        OAuthDiscoveryTimeout::Capped(DISCOVERY_TIMEOUT),
    )?;
    let authorization_manager =
        AuthorizationManager::new_with_oauth_http_client(url, Arc::new(oauth_http_client)).await?;
    match discover_streamable_http_oauth_with_manager(&authorization_manager).await? {
        Some(discovery) => Ok(Some(discovery)),
        // rmcp's standard metadata type requires an authorization endpoint.
        // Preserve the downstream device-only extension without bypassing the
        // upstream manager for normal OAuth or protected-resource discovery.
        None => discover_device_only_oauth_with_headers(url, default_headers).await,
    }
}

async fn discover_streamable_http_oauth_with_headers_and_http_client(
    url: &str,
    default_headers: HeaderMap,
    http_client: Arc<dyn HttpClient>,
    discovery_timeout: OAuthDiscoveryTimeout,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let oauth_http_client = OAuthHttpClientAdapter::new_scoped(
        http_client.clone(),
        default_headers.clone(),
        url,
        discovery_timeout,
    )?;
    let authorization_manager =
        AuthorizationManager::new_with_oauth_http_client(url, Arc::new(oauth_http_client)).await?;
    match discover_streamable_http_oauth_with_manager(&authorization_manager).await? {
        Some(discovery) => Ok(Some(discovery)),
        None => {
            discover_device_only_oauth_with_http_client(
                url,
                &default_headers,
                http_client.clone(),
                discovery_timeout,
            )
            .await
        }
    }
}

#[derive(Debug, Deserialize)]
struct DeviceOnlyOAuthDiscoveryMetadata {
    #[serde(default)]
    issuer: Option<String>,
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Option<Vec<String>>,
    #[serde(default)]
    device_authorization_endpoint: Option<String>,
    #[serde(default)]
    grant_types_supported: Option<Vec<String>>,
}

/// Binds direct device-only discovery to the issuer URL identified by its metadata path.
fn device_only_issuer_matches(
    resource_url: &str,
    metadata_path: &str,
    issuer: Option<&str>,
) -> bool {
    let Some(issuer) = issuer else {
        return false;
    };
    let Ok(parsed_issuer) = Url::parse(issuer) else {
        return false;
    };
    if !matches!(parsed_issuer.scheme(), "http" | "https")
        || !parsed_issuer.username().is_empty()
        || parsed_issuer.password().is_some()
        || parsed_issuer.query().is_some()
        || parsed_issuer.fragment().is_some()
    {
        return false;
    }
    // Keep the configured URL's original spelling. Parsing a URL before
    // comparing identifiers can normalize a different issuer into a match.
    let resource_identity = resource_url.split(['?', '#']).next().unwrap_or(resource_url);
    if metadata_path == "/.well-known/oauth-authorization-server" {
        let Some((scheme, remainder)) = resource_identity.split_once("://") else {
            return false;
        };
        let origin_end = scheme.len() + 3 + remainder.find('/').unwrap_or(remainder.len());
        let origin = &resource_identity[..origin_end];
        // The root metadata location is shared by these two issuer
        // identifiers. Accept either exact spelling at initial discovery.
        return issuer == origin || issuer.strip_suffix('/') == Some(origin);
    }
    issuer == resource_identity
}

fn device_only_discovery_from_metadata(
    metadata: DeviceOnlyOAuthDiscoveryMetadata,
) -> Option<StreamableHttpOAuthDiscovery> {
    if metadata.authorization_endpoint.is_some() {
        return None;
    }
    let device_authorization_endpoint = metadata.device_authorization_endpoint?;
    let token_endpoint = metadata.token_endpoint?;

    Some(StreamableHttpOAuthDiscovery {
        issuer: metadata.issuer,
        authorization_endpoint: None,
        token_endpoint,
        scopes_supported: normalize_scopes(metadata.scopes_supported),
        device_authorization_endpoint: Some(device_authorization_endpoint),
        grant_types_supported: normalize_scopes(metadata.grant_types_supported),
    })
}

fn discovery_from_authorization_metadata(
    metadata: AuthorizationMetadata,
) -> StreamableHttpOAuthDiscovery {
    let device_authorization_endpoint = metadata
        .additional_fields
        .get("device_authorization_endpoint")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    let grant_types_supported = metadata
        .additional_fields
        .get("grant_types_supported")
        .and_then(serde_json::Value::as_array)
        .map(|grant_types| {
            grant_types
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_string)
                .collect()
        });

    StreamableHttpOAuthDiscovery {
        issuer: metadata.issuer,
        authorization_endpoint: Some(metadata.authorization_endpoint),
        token_endpoint: metadata.token_endpoint,
        scopes_supported: normalize_scopes(metadata.scopes_supported),
        device_authorization_endpoint,
        grant_types_supported: normalize_scopes(grant_types_supported),
    }
}

async fn discover_device_only_oauth_with_headers(
    url: &str,
    default_headers: &HeaderMap,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let base_url = Url::parse(url)?;
    let builder = Client::builder()
        .timeout(DISCOVERY_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("OAuth metadata exceeded redirect limit");
            }
            if attempt
                .previous()
                .first()
                .is_some_and(|first| first.origin() == attempt.url().origin())
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }));
    let client = apply_default_headers(builder, default_headers).build()?;

    for candidate_path in discovery_paths(base_url.path()) {
        let mut discovery_url = base_url.clone();
        discovery_url.set_path(&candidate_path);
        let response = match client
            .get(discovery_url)
            .header(OAUTH_DISCOVERY_HEADER, OAUTH_DISCOVERY_VERSION)
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => continue,
        };
        if response.status() != StatusCode::OK {
            continue;
        }
        let metadata = match response.json::<DeviceOnlyOAuthDiscoveryMetadata>().await {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !device_only_issuer_matches(url, &candidate_path, metadata.issuer.as_deref()) {
            continue;
        }
        if let Some(discovery) = device_only_discovery_from_metadata(metadata) {
            return Ok(Some(discovery));
        }
    }

    Ok(None)
}

async fn discover_device_only_oauth_with_http_client(
    url: &str,
    default_headers: &HeaderMap,
    http_client: Arc<dyn HttpClient>,
    discovery_timeout: OAuthDiscoveryTimeout,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let base_url = Url::parse(url)?;
    let adapter = OAuthHttpClientAdapter::new_scoped(
        http_client,
        default_headers.clone(),
        url,
        discovery_timeout,
    )?;

    for candidate_path in discovery_paths(base_url.path()) {
        let mut discovery_url = base_url.clone();
        discovery_url.set_path(&candidate_path);
        let request = oauth2::http::Request::builder()
            .method("GET")
            .uri(discovery_url.as_str())
            .header(OAUTH_DISCOVERY_HEADER, OAUTH_DISCOVERY_VERSION)
            .body(Vec::new())?;
        let response = match adapter
            .execute_request(
                request,
                rmcp::transport::auth::OAuthHttpRedirectPolicy::Follow,
                /*timeout*/ None,
            )
            .await
        {
            Ok(response) => response,
            Err(_) => continue,
        };
        if response.status().as_u16() != StatusCode::OK.as_u16() {
            continue;
        }
        let metadata =
            match serde_json::from_slice::<DeviceOnlyOAuthDiscoveryMetadata>(response.body()) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
        if !device_only_issuer_matches(url, &candidate_path, metadata.issuer.as_deref()) {
            continue;
        }
        if let Some(discovery) = device_only_discovery_from_metadata(metadata) {
            return Ok(Some(discovery));
        }
    }

    Ok(None)
}

async fn discover_streamable_http_oauth_with_manager(
    authorization_manager: &AuthorizationManager,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    match discover_metadata(authorization_manager).boxed().await {
        Ok(metadata) => Ok(Some(discovery_from_authorization_metadata(metadata))),
        Err(AuthError::NoAuthorizationSupport) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn normalize_scopes(scopes_supported: Option<Vec<String>>) -> Option<Vec<String>> {
    let scopes_supported = scopes_supported?;

    let mut normalized = Vec::new();
    for scope in scopes_supported {
        let scope = scope.trim();
        if scope.is_empty() {
            continue;
        }
        let scope = scope.to_string();
        if !normalized.contains(&scope) {
            normalized.push(scope);
        }
    }

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Generates the narrow fallback paths used only for device-only metadata.
/// The normal OAuth and protected-resource paths are owned by rmcp.
fn discovery_paths(base_path: &str) -> Vec<String> {
    let trimmed = base_path.trim_start_matches('/').trim_end_matches('/');
    let canonical = "/.well-known/oauth-authorization-server".to_string();

    if trimmed.is_empty() {
        return vec![canonical];
    }

    let mut candidates = Vec::new();
    let mut push_unique = |candidate: String| {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    };

    push_unique(format!("{canonical}/{trimmed}"));
    push_unique(format!("/{trimmed}/.well-known/oauth-authorization-server"));
    push_unique(canonical);

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::Router;
    use axum::http::HeaderMap as AxumHeaderMap;
    use axum::http::HeaderValue;
    use axum::http::StatusCode as AxumStatusCode;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::IntoResponse;
    use axum::response::Response;
    use axum::routing::get;
    use codex_exec_server::ExecServerError;
    use codex_exec_server::HttpRequestParams;
    use codex_exec_server::HttpRequestResponse;
    use codex_exec_server::HttpResponseBodyStream;
    use futures::future::BoxFuture;
    use pretty_assertions::assert_eq;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::sync::Mutex;
    use tokio::task::JoinHandle;

    #[test]
    fn device_only_issuer_is_bound_to_the_discovery_location() {
        let resource = "https://mcp.example/mcp";
        let scoped = "/.well-known/oauth-authorization-server/mcp";
        let root = "/.well-known/oauth-authorization-server";
        assert!(device_only_issuer_matches(
            resource,
            scoped,
            Some("https://mcp.example/mcp")
        ));
        assert!(device_only_issuer_matches(
            resource,
            root,
            Some("https://mcp.example")
        ));
        assert!(device_only_issuer_matches(
            resource,
            root,
            Some("https://mcp.example/")
        ));
        assert!(!device_only_issuer_matches(
            resource,
            scoped,
            Some("https://MCP.EXAMPLE/mcp")
        ));
        assert!(!device_only_issuer_matches(
            resource,
            scoped,
            Some("https://mcp.example:443/mcp")
        ));
        let explicit = "https://MCP.EXAMPLE:443/mcp";
        assert!(device_only_issuer_matches(
            explicit,
            scoped,
            Some("https://MCP.EXAMPLE:443/mcp")
        ));
        assert!(!device_only_issuer_matches(
            explicit,
            scoped,
            Some("https://mcp.example/mcp")
        ));
        assert!(!device_only_issuer_matches(resource, scoped, None));
        assert!(!device_only_issuer_matches(
            resource,
            scoped,
            Some("https://other.example/mcp")
        ));
        assert!(!device_only_issuer_matches(
            resource,
            scoped,
            Some("https://mcp.example")
        ));
        assert!(!device_only_issuer_matches(
            resource,
            scoped,
            Some("https://user@mcp.example/mcp")
        ));
    }

    struct TestServer {
        url: String,
        handle: JoinHandle<()>,
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
    }

    fn json_response(value: serde_json::Value) -> Response {
        ([(CONTENT_TYPE, "application/json")], value.to_string()).into_response()
    }

    #[derive(Default)]
    struct RecordingHttpClient {
        timeout_ms: Mutex<Option<Option<u64>>>,
    }

    impl HttpClient for RecordingHttpClient {
        fn http_request(
            &self,
            _params: HttpRequestParams,
        ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
            Box::pin(async {
                Err(ExecServerError::HttpRequest(
                    "unexpected buffered request".to_string(),
                ))
            })
        }

        fn http_request_stream(
            &self,
            params: HttpRequestParams,
        ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>>
        {
            *self
                .timeout_ms
                .lock()
                .expect("timeout recorder lock should not be poisoned") = Some(params.timeout_ms);
            Box::pin(async {
                Err(ExecServerError::HttpRequest(
                    "expected discovery request failure".to_string(),
                ))
            })
        }
    }

    async fn spawn_oauth_discovery_server(mut metadata: serde_json::Value) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        metadata["issuer"] = serde_json::json!(format!("http://{address}/mcp"));
        let app = Router::new().route(
            "/.well-known/oauth-authorization-server/mcp",
            get({
                let metadata = metadata.clone();
                move || {
                    let metadata = metadata.clone();
                    async move { json_response(metadata) }
                }
            }),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });

        TestServer {
            url: format!("http://{address}/mcp"),
            handle,
        }
    }

    async fn spawn_protected_resource_oauth_server(mut metadata: serde_json::Value) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let base_url = format!("http://{address}");
        metadata["issuer"] = serde_json::json!(base_url);
        let resource_url = format!("{base_url}/mcp");
        let resource_metadata_url = format!("{base_url}/.well-known/oauth-protected-resource/mcp");
        let www_authenticate_value = HeaderValue::from_str(&format!(
            r#"Bearer resource_metadata="{resource_metadata_url}""#
        ))
        .expect("resource metadata URL should be a valid header value");
        let resource_metadata = serde_json::json!({
            "resource": resource_url,
            "authorization_servers": [base_url],
        });
        let app = Router::new()
            .route(
                "/mcp",
                get({
                    move || {
                        let www_authenticate_value = www_authenticate_value.clone();
                        async move {
                            let mut headers = AxumHeaderMap::new();
                            headers.insert(
                                axum::http::header::WWW_AUTHENTICATE,
                                www_authenticate_value,
                            );
                            (AxumStatusCode::UNAUTHORIZED, headers)
                        }
                    }
                }),
            )
            .route(
                "/.well-known/oauth-protected-resource/mcp",
                get({
                    move || {
                        let resource_metadata = resource_metadata.clone();
                        async move { json_response(resource_metadata) }
                    }
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get({
                    move || {
                        let metadata = metadata.clone();
                        async move { json_response(metadata) }
                    }
                }),
            );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });

        TestServer {
            url: resource_url,
            handle,
        }
    }

    struct EnvVarGuard {
        key: String,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &str, value: &str) -> Self {
            let original = std::env::var_os(key);
            unsafe {
                std::env::set_var(key, value);
            }
            Self {
                key: key.to_string(),
                original,
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.original {
                unsafe {
                    std::env::set_var(&self.key, value);
                }
            } else {
                unsafe {
                    std::env::remove_var(&self.key);
                }
            }
        }
    }

    #[tokio::test]
    async fn determine_auth_status_uses_bearer_token_when_authorization_header_present() {
        let status = determine_streamable_http_auth_status(
            "server",
            "not-a-url",
            /*bearer_token_env_var*/ None,
            Some(HashMap::from([(
                "Authorization".to_string(),
                "Bearer token".to_string(),
            )])),
            /*env_http_headers*/ None,
            OAuthCredentialsStoreMode::Keyring,
            AuthKeyringBackendKind::default(),
        )
        .await
        .expect("status should compute");

        assert_eq!(status, McpAuthState::BearerToken);
    }

    #[tokio::test]
    #[serial(auth_status_env)]
    async fn determine_auth_status_uses_bearer_token_when_env_authorization_header_present() {
        let _guard = EnvVarGuard::set("CODEX_RMCP_CLIENT_AUTH_STATUS_TEST_TOKEN", "Bearer token");
        let status = determine_streamable_http_auth_status(
            "server",
            "not-a-url",
            /*bearer_token_env_var*/ None,
            /*http_headers*/ None,
            Some(HashMap::from([(
                "Authorization".to_string(),
                "CODEX_RMCP_CLIENT_AUTH_STATUS_TEST_TOKEN".to_string(),
            )])),
            OAuthCredentialsStoreMode::Keyring,
            AuthKeyringBackendKind::default(),
        )
        .await
        .expect("status should compute");

        assert_eq!(status, McpAuthState::BearerToken);
    }

    #[tokio::test]
    async fn discover_streamable_http_oauth_returns_normalized_scopes() {
        let server = spawn_oauth_discovery_server(serde_json::json!({
            "authorization_endpoint": "https://example.com/authorize",
            "token_endpoint": "https://example.com/token",
            "scopes_supported": ["profile", " email ", "profile", "", "   "],
            "device_authorization_endpoint": "https://example.com/device",
            "grant_types_supported": [
                "authorization_code",
                " urn:ietf:params:oauth:grant-type:device_code ",
                "authorization_code"
            ],
        }))
        .await;

        let discovery = discover_streamable_http_oauth(
            &server.url,
            /*http_headers*/ None,
            /*env_http_headers*/ None,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                issuer: Some(server.url.clone()),
                authorization_endpoint: Some("https://example.com/authorize".to_string()),
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: Some(vec!["profile".to_string(), "email".to_string()]),
                device_authorization_endpoint: Some("https://example.com/device".to_string()),
                grant_types_supported: Some(vec![
                    "authorization_code".to_string(),
                    "urn:ietf:params:oauth:grant-type:device_code".to_string()
                ]),
            }
        );
    }

    #[tokio::test]
    async fn routed_oauth_discovery_caps_local_discovery_timeout() {
        let http_client = Arc::new(RecordingHttpClient::default());

        let discovery = discover_streamable_http_oauth_with_http_client(
            "http://example.com/mcp",
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            http_client.clone(),
            OAuthDiscoveryTimeout::LOCAL,
        )
        .await;

        let error = discovery.expect_err("transport failures must remain discovery errors");
        assert!(matches!(
            error.downcast_ref::<AuthError>(),
            Some(AuthError::MetadataError(message))
                if message.contains("expected discovery request failure")
        ));
        assert_eq!(
            *http_client
                .timeout_ms
                .lock()
                .expect("timeout recorder lock should not be poisoned"),
            Some(Some(
                u64::try_from(DISCOVERY_TIMEOUT.as_millis())
                    .expect("discovery timeout should fit in u64")
            ))
        );
    }

    #[tokio::test]
    async fn routed_oauth_discovery_preserves_requested_timeout() {
        let http_client = Arc::new(RecordingHttpClient::default());

        let discovery = discover_streamable_http_oauth_with_http_client(
            "http://example.com/mcp",
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            http_client.clone(),
            OAuthDiscoveryTimeout::Requested,
        )
        .await;

        let error = discovery.expect_err("transport failures must remain discovery errors");
        assert!(matches!(
            error.downcast_ref::<AuthError>(),
            Some(AuthError::MetadataError(message))
                if message.contains("expected discovery request failure")
        ));
        assert_eq!(
            *http_client
                .timeout_ms
                .lock()
                .expect("timeout recorder lock should not be poisoned"),
            Some(Some(30_000))
        );
    }
    #[tokio::test]
    async fn discover_streamable_http_oauth_ignores_empty_scopes() {
        let server = spawn_oauth_discovery_server(serde_json::json!({
            "authorization_endpoint": "https://example.com/authorize",
            "token_endpoint": "https://example.com/token",
            "scopes_supported": ["", "   "],
        }))
        .await;

        let discovery = discover_streamable_http_oauth(
            &server.url,
            /*http_headers*/ None,
            /*env_http_headers*/ None,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                issuer: Some(server.url.clone()),
                authorization_endpoint: Some("https://example.com/authorize".to_string()),
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: None,
                device_authorization_endpoint: None,
                grant_types_supported: None,
            }
        );
    }

    #[tokio::test]
    async fn supports_oauth_login_does_not_require_scopes_supported() {
        let server = spawn_oauth_discovery_server(serde_json::json!({
            "authorization_endpoint": "https://example.com/authorize",
            "token_endpoint": "https://example.com/token",
        }))
        .await;

        let supported = supports_oauth_login(&server.url)
            .await
            .expect("support check should succeed");

        assert!(supported);
    }

    #[tokio::test]
    async fn discover_streamable_http_oauth_follows_protected_resource_metadata() {
        let server = spawn_protected_resource_oauth_server(serde_json::json!({
            "authorization_endpoint": "https://example.com/authorize",
            "token_endpoint": "https://example.com/token",
            "scopes_supported": ["profile"],
        }))
        .await;

        let discovery = discover_streamable_http_oauth(
            &server.url,
            /*http_headers*/ None,
            /*env_http_headers*/ None,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                issuer: Some(server.url.trim_end_matches("/mcp").to_string()),
                authorization_endpoint: Some("https://example.com/authorize".to_string()),
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: Some(vec!["profile".to_string()]),
                device_authorization_endpoint: None,
                grant_types_supported: None,
            }
        );
    }

    #[tokio::test]
    async fn discover_streamable_http_oauth_accepts_device_only_metadata() {
        let server = spawn_oauth_discovery_server(serde_json::json!({
            "token_endpoint": "https://example.com/token",
            "device_authorization_endpoint": "https://example.com/device",
            "grant_types_supported": ["urn:ietf:params:oauth:grant-type:device_code"],
        }))
        .await;

        let discovery = discover_streamable_http_oauth(
            &server.url,
            /*http_headers*/ None,
            /*env_http_headers*/ None,
        )
        .await
        .expect("discovery should succeed")
        .expect("device-only oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                issuer: Some(server.url.clone()),
                authorization_endpoint: None,
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: None,
                device_authorization_endpoint: Some("https://example.com/device".to_string()),
                grant_types_supported: Some(vec![
                    "urn:ietf:params:oauth:grant-type:device_code".to_string()
                ]),
            }
        );
    }

    #[tokio::test]
    async fn runtime_oauth_discovery_preserves_device_only_metadata() {
        let server = spawn_oauth_discovery_server(serde_json::json!({
            "token_endpoint": "https://example.com/token",
            "device_authorization_endpoint": "https://example.com/device",
            "grant_types_supported": ["urn:ietf:params:oauth:grant-type:device_code"],
        }))
        .await;
        let discovery = discover_streamable_http_oauth_with_http_client(
            &server.url,
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            Arc::new(codex_exec_server::RouteAwareHttpClient::new(
                codex_http_client::HttpClientFactory::new(
                    codex_http_client::OutboundProxyPolicy::ReqwestDefault,
                ),
            )),
            OAuthDiscoveryTimeout::LOCAL,
        )
        .await
        .expect("runtime discovery should succeed")
        .expect("device-only oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                issuer: Some(server.url.clone()),
                authorization_endpoint: None,
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: None,
                device_authorization_endpoint: Some("https://example.com/device".to_string()),
                grant_types_supported: Some(vec![
                    "urn:ietf:params:oauth:grant-type:device_code".to_string()
                ]),
            }
        );
    }
}
