use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpHeader;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use codex_protocol::protocol::McpAuthStatus;
use futures::FutureExt;
use reqwest::StatusCode;
use reqwest::Url;
use reqwest::header::AUTHORIZATION;
use reqwest::header::HeaderMap;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::auth::AuthError;
use rmcp::transport::auth::AuthorizationMetadata;
use serde::Deserialize;
use tracing::debug;

use crate::http_client_adapter::StreamableHttpRedirectMode;
use crate::oauth::StoredOAuthTokenStatus;
use crate::oauth::oauth_token_status;
use crate::oauth_callback::McpOAuthCallbackMode;
use crate::oauth_callback::callback_mode;
use crate::oauth_http_client::OAuthHttpClientAdapter;
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
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: String,
    pub scopes_supported: Option<Vec<String>>,
    pub device_authorization_endpoint: Option<String>,
    pub grant_types_supported: Option<Vec<String>>,
    pub callback_mode: McpOAuthCallbackMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpLoginRequirement {
    Login,
    Reauthentication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthState {
    Unsupported,
    Unknown,
    LoggedOut(McpLoginRequirement),
    BearerToken,
    OAuth,
}

impl From<McpAuthState> for McpAuthStatus {
    fn from(value: McpAuthState) -> Self {
        match value {
            McpAuthState::Unsupported => Self::Unsupported,
            McpAuthState::Unknown => Self::Unknown,
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

/// Determine authentication status while routing OAuth discovery through the
/// provided HTTP client.
#[allow(clippy::too_many_arguments)]
pub async fn determine_streamable_http_auth_status(
    server_name: &str,
    url: &str,
    bearer_token_env_var: Option<&str>,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_client: Arc<dyn HttpClient>,
    discovery_timeout: OAuthDiscoveryTimeout,
    redirect_mode: StreamableHttpRedirectMode,
) -> Result<McpAuthState> {
    let has_configured_headers = has_configured_headers(&http_headers, &env_http_headers);
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
            has_configured_headers,
            redirect_mode,
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
            Err(error)
        }
    }
}

pub async fn discover_streamable_http_oauth(
    url: &str,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    http_client: Arc<dyn HttpClient>,
    discovery_timeout: OAuthDiscoveryTimeout,
    redirect_mode: StreamableHttpRedirectMode,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let has_configured_headers = has_configured_headers(&http_headers, &env_http_headers);
    let default_headers = build_default_headers(http_headers, env_http_headers)?;
    discover_streamable_http_oauth_with_headers_and_http_client(
        url,
        default_headers,
        http_client,
        discovery_timeout,
        has_configured_headers,
        redirect_mode,
    )
    .await
}

async fn discover_streamable_http_oauth_with_headers_and_http_client(
    url: &str,
    default_headers: HeaderMap,
    http_client: Arc<dyn HttpClient>,
    discovery_timeout: OAuthDiscoveryTimeout,
    has_configured_headers: bool,
    redirect_mode: StreamableHttpRedirectMode,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let oauth_http_client = match discovery_timeout {
        OAuthDiscoveryTimeout::Requested => OAuthHttpClientAdapter::new_with_redirect_mode(
            http_client,
            default_headers,
            url,
            has_configured_headers,
            redirect_mode,
        )?,
        OAuthDiscoveryTimeout::Capped(max_timeout) => {
            OAuthHttpClientAdapter::new_with_max_timeout_and_redirect_mode(
                http_client,
                default_headers,
                url,
                max_timeout,
                has_configured_headers,
                redirect_mode,
            )?
        }
    };
    let mut authorization_manager =
        AuthorizationManager::new_with_oauth_http_client(url, Arc::new(oauth_http_client)).await?;
    match discover_streamable_http_oauth_with_manager(&authorization_manager).await? {
        Some(discovery) => Ok(Some(discovery)),
        None => {
            discover_device_only_oauth_with_http_client(
                url,
                &default_headers,
                http_client.as_ref(),
                discovery_timeout,
                has_configured_headers,
                redirect_mode,
            )
            .await
        }
    }
}

#[derive(Debug, Deserialize)]
struct DeviceOnlyOAuthDiscoveryMetadata {
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

fn device_only_discovery_from_metadata(
    metadata: DeviceOnlyOAuthDiscoveryMetadata,
) -> Option<StreamableHttpOAuthDiscovery> {
    if metadata.authorization_endpoint.is_some() {
        return None;
    }
    let device_authorization_endpoint = metadata.device_authorization_endpoint?;
    let token_endpoint = metadata.token_endpoint?;

    Some(StreamableHttpOAuthDiscovery {
        authorization_endpoint: None,
        token_endpoint,
        scopes_supported: normalize_scopes(metadata.scopes_supported),
        device_authorization_endpoint: Some(device_authorization_endpoint),
        grant_types_supported: normalize_scopes(metadata.grant_types_supported),
        callback_mode: McpOAuthCallbackMode::CallbackSpecific,
    })
}

fn discovery_from_authorization_metadata(
    metadata: AuthorizationMetadata,
) -> StreamableHttpOAuthDiscovery {
    let callback_mode = callback_mode(&metadata).unwrap_or(McpOAuthCallbackMode::CallbackSpecific);
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
        authorization_endpoint: Some(metadata.authorization_endpoint),
        token_endpoint: metadata.token_endpoint,
        scopes_supported: normalize_scopes(metadata.scopes_supported),
        device_authorization_endpoint,
        grant_types_supported: normalize_scopes(grant_types_supported),
        callback_mode,
    }
}

fn oauth_discovery_protocol_headers(default_headers: &HeaderMap) -> Result<Vec<HttpHeader>> {
    let mut headers = default_headers
        .iter()
        .map(|(name, value)| {
            Ok(HttpHeader {
                name: name.as_str().to_string(),
                value: value.to_str()?.to_string(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    headers.push(HttpHeader {
        name: OAUTH_DISCOVERY_HEADER.to_string(),
        value: OAUTH_DISCOVERY_VERSION.to_string(),
    });
    Ok(headers)
}

async fn discover_device_only_oauth_with_http_client(
    url: &str,
    default_headers: &HeaderMap,
    http_client: &dyn HttpClient,
    discovery_timeout: OAuthDiscoveryTimeout,
    has_configured_headers: bool,
    redirect_mode: StreamableHttpRedirectMode,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    let base_url = Url::parse(url)?;
    let timeout_ms = match discovery_timeout {
        OAuthDiscoveryTimeout::Requested => None,
        OAuthDiscoveryTimeout::Capped(timeout) => {
            Some(timeout.as_millis().try_into().unwrap_or(u64::MAX))
        }
    };

    for (index, candidate_path) in discovery_paths(base_url.path()).into_iter().enumerate() {
        let mut discovery_url = base_url.clone();
        discovery_url.set_path(&candidate_path);
        let response = match http_client
            .http_request(HttpRequestParams {
                method: "GET".to_string(),
                url: discovery_url.to_string(),
                headers: oauth_discovery_protocol_headers(default_headers)?,
                body: None,
                timeout_ms,
                redirect_policy: if redirect_mode == StreamableHttpRedirectMode::AgentPluginV1
                    && (has_configured_headers || default_headers.contains_key(AUTHORIZATION))
                {
                    HttpRedirectPolicy::Stop
                } else {
                    HttpRedirectPolicy::Follow
                },
                request_id: format!("oauth-device-only-discovery-{index}"),
                stream_response: false,
            })
            .await
        {
            Ok(response) => response,
            Err(_) => continue,
        };
        if response.status != StatusCode::OK.as_u16() {
            continue;
        }
        let metadata = match serde_json::from_slice::<DeviceOnlyOAuthDiscoveryMetadata>(
            &response.body.into_inner(),
        ) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if let Some(discovery) = device_only_discovery_from_metadata(metadata) {
            return Ok(Some(discovery));
        }
    }

    Ok(None)
}

fn has_configured_headers(
    http_headers: &Option<HashMap<String, String>>,
    env_http_headers: &Option<HashMap<String, String>>,
) -> bool {
    http_headers
        .as_ref()
        .is_some_and(|headers| !headers.is_empty())
        || env_http_headers
            .as_ref()
            .is_some_and(|headers| !headers.is_empty())
}

async fn discover_streamable_http_oauth_with_manager(
    authorization_manager: &AuthorizationManager,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    match authorization_manager.resolve_metadata().boxed().await {
        Ok(resolution) if !resolution.source.is_discovered() => Ok(None),
        Ok(resolution) => Ok(Some(discovery_from_authorization_metadata(
            resolution.metadata,
        ))),
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
    use codex_exec_server::HttpRedirectPolicy;
    use codex_exec_server::HttpRequestParams;
    use codex_exec_server::HttpRequestResponse;
    use codex_exec_server::HttpResponseBodyStream;
    use codex_exec_server::RouteAwareHttpClient;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use futures::future::BoxFuture;
    use pretty_assertions::assert_eq;
    use serial_test::serial;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::sync::Mutex;
    use tokio::task::JoinHandle;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    struct TestServer {
        url: String,
        handle: JoinHandle<()>,
    }

    fn test_http_client() -> Arc<dyn HttpClient> {
        Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
            OutboundProxyPolicy::ReqwestDefault,
        )))
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
        headers: Mutex<Option<Vec<(String, String)>>>,
        redirect_policy: Mutex<Option<HttpRedirectPolicy>>,
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
                .headers
                .lock()
                .expect("header recorder lock should not be poisoned") = Some(
                params
                    .headers
                    .iter()
                    .map(|header| (header.name.clone(), header.value.clone()))
                    .collect(),
            );
            *self
                .timeout_ms
                .lock()
                .expect("timeout recorder lock should not be poisoned") = Some(params.timeout_ms);
            *self
                .redirect_policy
                .lock()
                .expect("redirect policy recorder lock should not be poisoned") =
                Some(params.redirect_policy);
            Box::pin(async {
                Err(ExecServerError::HttpRequest(
                    "expected discovery request failure".to_string(),
                ))
            })
        }
    }

    struct DeviceOnlyHttpClient {
        redirect_policy: Arc<Mutex<Option<HttpRedirectPolicy>>>,
    }

    impl HttpClient for DeviceOnlyHttpClient {
        fn http_request(
            &self,
            params: HttpRequestParams,
        ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
            *self
                .redirect_policy
                .lock()
                .expect("device-only redirect recorder lock should not be poisoned") =
                Some(params.redirect_policy);
            Box::pin(async {
                Ok(HttpRequestResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: serde_json::to_vec(&serde_json::json!({
                        "token_endpoint": "https://example.com/token",
                        "device_authorization_endpoint": "https://example.com/device",
                        "grant_types_supported": ["urn:ietf:params:oauth:grant-type:device_code"],
                    }))
                    .expect("device-only metadata should serialize")
                    .into(),
                })
            })
        }

        fn http_request_stream(
            &self,
            _params: HttpRequestParams,
        ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>>
        {
            Box::pin(async {
                Err(ExecServerError::HttpRequest(
                    "device-only metadata requires the narrow fallback".to_string(),
                ))
            })
        }
    }

    fn assert_recorded_discovery_failure(discovery: Result<Option<StreamableHttpOAuthDiscovery>>) {
        let error = discovery.expect_err("the recording HTTP client rejects OAuth discovery");
        assert!(
            matches!(
                error.downcast_ref::<AuthError>(),
                Some(AuthError::MetadataError(reason))
                    if reason.contains("expected discovery request failure")
            ),
            "OAuth discovery must preserve the executor transport failure: {error:#}"
        );
    }

    async fn spawn_oauth_discovery_server(metadata: serde_json::Value) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let mut metadata = metadata;
        if let Some(metadata) = metadata.as_object_mut() {
            metadata
                .entry("issuer")
                .or_insert_with(|| format!("http://{address}/mcp").into());
        }
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

    async fn spawn_protected_resource_oauth_server(metadata: serde_json::Value) -> TestServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let base_url = format!("http://{address}");
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
            test_http_client(),
            OAuthDiscoveryTimeout::Requested,
            StreamableHttpRedirectMode::Legacy,
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
            test_http_client(),
            OAuthDiscoveryTimeout::Requested,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("status should compute");

        assert_eq!(status, McpAuthState::BearerToken);
    }

    #[tokio::test]
    async fn oauth_metadata_preserves_login_without_probing_anonymous_tools() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let metadata = serde_json::json!({
            "issuer": format!("http://{address}/mcp"),
            "authorization_endpoint": format!("http://{address}/authorize"),
            "token_endpoint": format!("http://{address}/token"),
        });
        let app = Router::new()
            .route(
                "/mcp",
                get(|| async { StatusCode::METHOD_NOT_ALLOWED }).post(
                    |Json(request): Json<serde_json::Value>| async move {
                        let result = match request["method"].as_str() {
                            Some("initialize") => serde_json::json!({
                                "protocolVersion": "2024-11-05",
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "oauth", "version": "1"},
                            }),
                            Some("tools/list") => serde_json::json!({"tools": []}),
                            _ => serde_json::json!({}),
                        };
                        Json(serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": request["id"],
                            "result": result,
                        }))
                    },
                ),
            )
            .route(
                "/.well-known/oauth-authorization-server/mcp",
                get(move || async move { Json(metadata) }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });
        let url = format!("http://{address}/mcp");
        let discovery = discover_streamable_http_oauth(
            &url,
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            test_http_client(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await;
        assert_eq!(
            determine_auth_status_from_discovery("server", &url, discovery)
                .expect("auth status should compute"),
            McpAuthState::LoggedOut(McpLoginRequirement::Login)
        );
        server.abort();
    }

    #[tokio::test]
    async fn oauth_discovery_does_not_follow_cross_origin_redirects() {
        let redirect_target = MockServer::start().await;
        let redirect_url = format!("{}/redirect-target", redirect_target.uri());
        Mock::given(method("GET"))
            .and(path("/redirect-target"))
            .and(header("x-api-key", "sensitive-key"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&redirect_target)
            .await;

        let resource_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/mcp"))
            .and(header("x-api-key", "sensitive-key"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", redirect_url.clone()),
            )
            .expect(1)
            .mount(&resource_server)
            .await;

        let error = discover_streamable_http_oauth(
            &format!("{}/mcp", resource_server.uri()),
            Some(HashMap::from([(
                "x-api-key".to_string(),
                "sensitive-key".to_string(),
            )])),
            /*env_http_headers*/ None,
            test_http_client(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect_err("cross-origin OAuth discovery redirects must be rejected");

        assert!(
            matches!(
                error.downcast_ref::<AuthError>(),
                Some(AuthError::MetadataError(reason))
                    if reason.contains("OAuth discovery redirect to non-same-origin URL rejected")
                        && reason.contains(&redirect_url)
            ),
            "OAuth discovery must preserve the cross-origin redirect rejection: {error:#}"
        );
        redirect_target.verify().await;
        resource_server.verify().await;
    }

    #[tokio::test]
    async fn determine_auth_status_preserves_transient_http_errors() {
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_EARLY,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/mcp"))
                .respond_with(ResponseTemplate::new(status.as_u16()))
                .expect(1)
                .mount(&server)
                .await;

            let error = determine_streamable_http_auth_status(
                "transient-http-error",
                &format!("{}/mcp", server.uri()),
                /*bearer_token_env_var*/ None,
                /*http_headers*/ None,
                /*env_http_headers*/ None,
                OAuthCredentialsStoreMode::File,
                AuthKeyringBackendKind::default(),
                test_http_client(),
                OAuthDiscoveryTimeout::LOCAL,
                StreamableHttpRedirectMode::Legacy,
            )
            .await
            .expect_err("transient OAuth discovery failures must not become unsupported access");

            assert!(
                matches!(
                    error.downcast_ref::<AuthError>(),
                    Some(AuthError::MetadataError(reason)) if reason.contains(status.as_str())
                ),
                "auth-status discovery must preserve HTTP {status}: {error:#}"
            );
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn discover_streamable_http_oauth_returns_normalized_scopes() {
        let server = spawn_oauth_discovery_server(serde_json::json!({
            "authorization_endpoint": "https://example.com/authorize",
            "token_endpoint": "https://example.com/token",
            "authorization_response_iss_parameter_supported": true,
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
            test_http_client(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                authorization_endpoint: Some("https://example.com/authorize".to_string()),
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: Some(vec!["profile".to_string(), "email".to_string()]),
                device_authorization_endpoint: Some("https://example.com/device".to_string()),
                grant_types_supported: Some(vec![
                    "authorization_code".to_string(),
                    "urn:ietf:params:oauth:grant-type:device_code".to_string()
                ]),
                callback_mode: McpOAuthCallbackMode::IssuerBound,
            }
        );
    }

    #[tokio::test]
    async fn issuer_support_without_a_metadata_issuer_falls_back_to_distinct_callbacks() {
        let server = spawn_oauth_discovery_server(serde_json::json!({
            "issuer": null,
            "authorization_endpoint": "https://example.com/authorize",
            "token_endpoint": "https://example.com/token",
            "authorization_response_iss_parameter_supported": true,
        }))
        .await;

        let discovery = discover_streamable_http_oauth(
            &server.url,
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            test_http_client(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                authorization_endpoint: Some("https://example.com/authorize".to_string()),
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: None,
                device_authorization_endpoint: None,
                grant_types_supported: None,
                callback_mode: McpOAuthCallbackMode::CallbackSpecific,
            }
        );
    }

    #[tokio::test]
    async fn routed_oauth_discovery_caps_local_discovery_timeout() {
        let http_client = Arc::new(RecordingHttpClient::default());

        let discovery = discover_streamable_http_oauth(
            "http://example.com/mcp",
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            http_client.clone(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await;

        assert_recorded_discovery_failure(discovery);
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

        let discovery = discover_streamable_http_oauth(
            "http://example.com/mcp",
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            http_client.clone(),
            OAuthDiscoveryTimeout::Requested,
            StreamableHttpRedirectMode::Legacy,
        )
        .await;

        assert_recorded_discovery_failure(discovery);
        assert_eq!(
            *http_client
                .timeout_ms
                .lock()
                .expect("timeout recorder lock should not be poisoned"),
            Some(Some(30_000))
        );
    }

    #[tokio::test]
    async fn routed_agent_plugin_oauth_discovery_stops_with_configured_headers() {
        let http_client = Arc::new(RecordingHttpClient::default());

        let discovery = discover_streamable_http_oauth(
            "http://example.com/mcp",
            Some(HashMap::from([(
                "X-Mcp-Discovery".to_string(),
                "configured-value".to_string(),
            )])),
            /*env_http_headers*/ None,
            http_client.clone(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::AgentPluginV1,
        )
        .await;

        assert_recorded_discovery_failure(discovery);
        let headers = http_client
            .headers
            .lock()
            .expect("header recorder lock should not be poisoned")
            .clone()
            .expect("discovery should issue an HTTP request");
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("x-mcp-discovery"))
                .map(|(_, value)| value.as_str()),
            Some("configured-value")
        );
        assert_eq!(
            *http_client
                .redirect_policy
                .lock()
                .expect("redirect policy recorder lock should not be poisoned"),
            Some(HttpRedirectPolicy::Stop)
        );
    }

    #[tokio::test]
    async fn discover_streamable_http_oauth_follows_protected_resource_metadata() {
        let authorization_server = spawn_oauth_discovery_server(serde_json::json!({
            "authorization_endpoint": "https://example.com/authorize",
            "token_endpoint": "https://example.com/token",
            "scopes_supported": ["read", " write ", "read"],
        }))
        .await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener.local_addr().expect("listener should have address");
        let resource_metadata_url = format!("http://{address}/oauth-resource");
        let challenge = format!("Bearer resource_metadata=\"{resource_metadata_url}\"");
        let authorization_server_url = authorization_server.url.clone();
        let app = Router::new()
            .route(
                "/mcp",
                get(move || {
                    let challenge = challenge.clone();
                    async move { (StatusCode::UNAUTHORIZED, [(WWW_AUTHENTICATE, challenge)]) }
                }),
            )
            .route(
                "/oauth-resource",
                get(move || {
                    let authorization_server_url = authorization_server_url.clone();
                    async move {
                        Json(serde_json::json!({
                            "resource": format!("http://{address}/mcp"),
                            "authorization_servers": [authorization_server_url],
                        }))
                    }
                }),
            );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("server should run");
        });
        let resource_server = TestServer {
            url: format!("http://{address}/mcp"),
            handle,
        };

        let discovery = discover_streamable_http_oauth(
            &resource_server.url,
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            test_http_client(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(
            discovery.scopes_supported,
            Some(vec!["read".to_string(), "write".to_string()])
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
            test_http_client(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                authorization_endpoint: Some("https://example.com/authorize".to_string()),
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: None,
                device_authorization_endpoint: None,
                grant_types_supported: None,
                callback_mode: McpOAuthCallbackMode::CallbackSpecific,
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

        let supported = discover_streamable_http_oauth(
            &server.url,
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            test_http_client(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("support check should succeed")
        .is_some();

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
            test_http_client(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                authorization_endpoint: Some("https://example.com/authorize".to_string()),
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: Some(vec!["profile".to_string()]),
                device_authorization_endpoint: None,
                grant_types_supported: None,
                callback_mode: McpOAuthCallbackMode::CallbackSpecific,
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
            test_http_client(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("discovery should succeed")
        .expect("device-only oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                authorization_endpoint: None,
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: None,
                device_authorization_endpoint: Some("https://example.com/device".to_string()),
                grant_types_supported: Some(vec![
                    "urn:ietf:params:oauth:grant-type:device_code".to_string()
                ]),
                callback_mode: McpOAuthCallbackMode::CallbackSpecific,
            }
        );
    }

    #[tokio::test]
    async fn runtime_oauth_discovery_preserves_device_only_metadata() {
        let http_client = Arc::new(DeviceOnlyHttpClient {
            redirect_policy: Arc::new(Mutex::new(None)),
        });
        let discovery = discover_streamable_http_oauth(
            "http://example.com/mcp",
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            http_client.clone(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::AgentPluginV1,
        )
        .await
        .expect("runtime discovery should succeed")
        .expect("device-only oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                authorization_endpoint: None,
                token_endpoint: "https://example.com/token".to_string(),
                scopes_supported: None,
                device_authorization_endpoint: Some("https://example.com/device".to_string()),
                grant_types_supported: Some(vec![
                    "urn:ietf:params:oauth:grant-type:device_code".to_string()
                ]),
                callback_mode: McpOAuthCallbackMode::CallbackSpecific,
            }
        );
        assert_eq!(
            *http_client
                .redirect_policy
                .lock()
                .expect("device-only redirect recorder lock should not be poisoned"),
            Some(HttpRedirectPolicy::Follow)
        );

        let configured_client = Arc::new(DeviceOnlyHttpClient {
            redirect_policy: Arc::new(Mutex::new(None)),
        });
        let _ = discover_streamable_http_oauth(
            "http://example.com/mcp",
            Some(HashMap::from([(
                "X-Test".to_string(),
                "configured".to_string(),
            )])),
            /*env_http_headers*/ None,
            configured_client.clone(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::AgentPluginV1,
        )
        .await;
        assert_eq!(
            *configured_client
                .redirect_policy
                .lock()
                .expect("device-only redirect recorder lock should not be poisoned"),
            Some(HttpRedirectPolicy::Stop)
        );
    }
}
