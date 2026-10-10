use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use codex_exec_server::HttpClient;
use codex_protocol::protocol::McpAuthStatus;
use futures::FutureExt;
use http::HeaderMap;
use http::header::AUTHORIZATION;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::auth::AuthError;
use rmcp::transport::auth::AuthorizationMetadata;
use rmcp::transport::auth::AuthorizationMetadataResolution;
use tracing::debug;

use crate::http_client_adapter::StreamableHttpRedirectMode;
use crate::oauth::StoredOAuthTokenStatus;
use crate::oauth::oauth_token_status;
use crate::oauth_callback::McpOAuthCallbackMode;
use crate::oauth_callback::callback_mode;
use crate::oauth_http_client::DeviceMetadataReceiptCollector;
use crate::oauth_http_client::OAuthHttpClientAdapter;
use crate::utils::build_default_headers;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

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
    pub scopes_supported: Option<Vec<String>>,
    pub callback_mode: McpOAuthCallbackMode,
    pub device_authorization: Option<VerifiedDeviceAuthorization>,
}

/// An unforgeable, in-memory capability built from one verified discovery response.
#[derive(Debug, Clone)]
pub struct VerifiedDeviceAuthorization {
    metadata: AuthorizationMetadata,
}

impl VerifiedDeviceAuthorization {
    pub fn issuer(&self) -> &str {
        self.metadata.issuer.as_deref().unwrap_or_default()
    }

    pub fn device_authorization_endpoint(&self) -> Option<&str> {
        self.metadata
            .additional_fields
            .get("device_authorization_endpoint")
            .and_then(serde_json::Value::as_str)
    }

    pub fn token_endpoint(&self) -> &str {
        &self.metadata.token_endpoint
    }

    pub fn registration_endpoint(&self) -> Option<&str> {
        self.metadata.registration_endpoint.as_deref()
    }

    pub fn supports_device_code_grant(&self) -> bool {
        self.metadata
            .additional_fields
            .get("grant_types_supported")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|grants| {
                grants.iter().any(|grant| {
                    grant.as_str()
                        == Some("urn:ietf:params:oauth:grant-type:device_code")
                })
            })
    }

    pub fn supports_refresh_token(&self) -> bool {
        self.metadata
            .additional_fields
            .get("grant_types_supported")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|grants| {
                grants
                    .iter()
                    .any(|grant| grant.as_str() == Some("refresh_token"))
            })
    }

    #[cfg(test)]
    pub(crate) fn from_metadata_for_test(metadata: serde_json::Value) -> Self {
        Self {
            metadata: serde_json::from_value(metadata)
                .expect("synthetic test metadata should match AuthorizationMetadata"),
        }
    }
}

impl PartialEq for VerifiedDeviceAuthorization {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (serde_json::to_value(&self.metadata), serde_json::to_value(&other.metadata)),
            (Ok(left), Ok(right)) if left == right
        )
    }
}

impl Eq for VerifiedDeviceAuthorization {}

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
    let mut oauth_http_client = match discovery_timeout {
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
    let device_metadata_receipt = oauth_http_client.enable_device_metadata_receipt();
    let mut authorization_manager =
        AuthorizationManager::new_with_oauth_http_client(url, Arc::new(oauth_http_client)).await?;
    authorization_manager.set_allow_missing_issuer(true);
    discover_streamable_http_oauth_with_manager(&authorization_manager, &device_metadata_receipt)
        .await
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
    device_metadata_receipt: &DeviceMetadataReceiptCollector,
) -> Result<Option<StreamableHttpOAuthDiscovery>> {
    match authorization_manager.resolve_metadata().boxed().await {
        Ok(resolution) if !resolution.source.is_discovered() => Ok(None),
        Ok(resolution) => {
            let device_authorization =
                verified_device_authorization(&resolution, device_metadata_receipt);
            let metadata = resolution.metadata;
            Ok(Some(StreamableHttpOAuthDiscovery {
                callback_mode: callback_mode(&metadata)
                    .unwrap_or(McpOAuthCallbackMode::CallbackSpecific),
                scopes_supported: normalize_scopes(metadata.scopes_supported),
                device_authorization,
            }))
        }
        Err(AuthError::NoAuthorizationSupport) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn verified_device_authorization(
    resolution: &AuthorizationMetadataResolution,
    receipt: &DeviceMetadataReceiptCollector,
) -> Option<VerifiedDeviceAuthorization> {
    verified_device_authorization_from_metadata(
        &resolution.metadata,
        resolution.source.is_discovered(),
        receipt,
    )
}

fn verified_device_authorization_from_metadata(
    metadata: &AuthorizationMetadata,
    discovered: bool,
    receipt: &DeviceMetadataReceiptCollector,
) -> Option<VerifiedDeviceAuthorization> {
    let response = receipt.take_matching(metadata, discovered)?;
    let expected_issuer = expected_issuer_for_metadata_url(&response.request_url)?;
    let metadata_issuer = response.metadata.issuer.as_deref()?;
    if !issuer_identifiers_match(metadata_issuer, &expected_issuer)
        || !is_https_url(metadata_issuer)
    {
        return None;
    }
    let metadata = response.metadata;
    let device_endpoint = metadata
        .additional_fields
        .get("device_authorization_endpoint")
        .and_then(serde_json::Value::as_str)?;
    if device_endpoint.trim().is_empty()
        || !is_https_url(device_endpoint)
        || !is_https_url(&metadata.token_endpoint)
    {
        return None;
    }
    let has_device_code_grant = metadata
        .additional_fields
        .get("grant_types_supported")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|grants| {
            grants.iter().any(|grant| {
                grant.as_str() == Some("urn:ietf:params:oauth:grant-type:device_code")
            })
        });
    has_device_code_grant.then_some(VerifiedDeviceAuthorization { metadata })
}

#[derive(Clone, Copy)]
enum MetadataUrlStyle {
    Rfc8414,
    OidcWellKnownPrefix,
    OidcWellKnownSuffix,
}

fn expected_issuer_for_metadata_url(metadata_url: &url::Url) -> Option<String> {
    if metadata_url.scheme() != "https"
        || has_url_userinfo(metadata_url)
        || metadata_url.query().is_some()
        || metadata_url.fragment().is_some()
    {
        return None;
    }
    let path = metadata_url.path();
    let oauth_prefix = "/.well-known/oauth-authorization-server";
    let oidc_prefix = "/.well-known/openid-configuration";
    let (issuer_path, style) = if path == oauth_prefix {
        (String::new(), MetadataUrlStyle::Rfc8414)
    } else if let Some(suffix) = path.strip_prefix(&format!("{oauth_prefix}/")) {
        if !is_canonical_nested_issuer_path(suffix) {
            return None;
        }
        (format!("/{suffix}"), MetadataUrlStyle::Rfc8414)
    } else if path == oidc_prefix {
        (String::new(), MetadataUrlStyle::OidcWellKnownPrefix)
    } else if let Some(suffix) = path.strip_prefix(&format!("{oidc_prefix}/")) {
        if !is_canonical_nested_issuer_path(suffix) {
            return None;
        }
        (
            format!("/{suffix}"),
            MetadataUrlStyle::OidcWellKnownPrefix,
        )
    } else if let Some(issuer_path) = path.strip_suffix(oidc_prefix) {
        if !is_canonical_nested_issuer_path(issuer_path.strip_prefix('/')?) {
            return None;
        }
        (issuer_path.to_string(), MetadataUrlStyle::OidcWellKnownSuffix)
    } else {
        return None;
    };
    let mut expected_issuer = metadata_url.clone();
    expected_issuer.set_path(&issuer_path);
    let issuer_path = if expected_issuer.path() == "/" {
        ""
    } else {
        expected_issuer.path()
    };
    let canonical_metadata_path = match style {
        MetadataUrlStyle::Rfc8414 => format!("{oauth_prefix}{issuer_path}"),
        MetadataUrlStyle::OidcWellKnownPrefix => format!("{oidc_prefix}{issuer_path}"),
        MetadataUrlStyle::OidcWellKnownSuffix => format!("{issuer_path}{oidc_prefix}"),
    };
    let mut canonical_metadata_url = expected_issuer.clone();
    canonical_metadata_url.set_path(&canonical_metadata_path);
    if &canonical_metadata_url != metadata_url {
        return None;
    }
    Some(expected_issuer.to_string())
}

fn is_canonical_nested_issuer_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.ends_with('/')
        && !path.contains("//")
}

fn issuer_identifiers_match(received_issuer: &str, expected_issuer: &str) -> bool {
    trim_root_issuer_slash(received_issuer) == trim_root_issuer_slash(expected_issuer)
}

fn trim_root_issuer_slash(issuer: &str) -> &str {
    let Some(without_slash) = issuer.strip_suffix('/') else {
        return issuer;
    };
    if url::Url::parse(without_slash).is_ok_and(|url| url.path().is_empty() || url.path() == "/") {
        without_slash
    } else {
        issuer
    }
}

fn is_https_url(value: &str) -> bool {
    url::Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            && !has_url_userinfo(&url)
            && url.fragment().is_none()
    })
}

fn has_url_userinfo(url: &url::Url) -> bool {
    url.as_str()
        .split_once("://")
        .and_then(|(_, rest)| rest.split(|character| matches!(character, '/' | '?' | '#')).next())
        .is_none_or(|authority| authority.contains('@'))
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::Router;
    use axum::http::StatusCode;
    use axum::http::header::WWW_AUTHENTICATE;
    use axum::routing::get;
    use codex_exec_server::ExecServerError;
    use codex_exec_server::HttpHeader;
    use codex_exec_server::HttpRedirectPolicy;
    use codex_exec_server::HttpRequestParams;
    use codex_exec_server::HttpRequestResponse;
    use codex_exec_server::HttpResponseBodyStream;
    use codex_exec_server::RouteAwareHttpClient;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use futures::future::BoxFuture;
    use oauth2::TokenResponse;
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

    async fn test_http_client() -> Arc<dyn HttpClient> {
        let client: Arc<dyn HttpClient> = Arc::new(RouteAwareHttpClient::new(
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
        ));
        crate::oauth::test_support::warm_http_client(client.as_ref())
            .await
            .expect("warm production HTTP client before discovery deadlines");
        client
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.handle.abort();
        }
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

    #[derive(Clone, Copy)]
    enum SyntheticDiscoveryScenario {
        Direct,
        OidcFallback,
        SameOriginRedirect,
    }

    #[derive(Clone)]
    struct SyntheticDeviceAuthHttpClient {
        scenario: SyntheticDiscoveryScenario,
        requests: Arc<Mutex<Vec<String>>>,
    }

    impl SyntheticDeviceAuthHttpClient {
        const RESOURCE_URL: &'static str = "https://resource-a.example.test/mcp";
        const ISSUER: &'static str = "https://issuer-a.example.test/tenant";
        const DEVICE_ENDPOINT: &'static str = "https://device-b.example.test/device";
        const TOKEN_ENDPOINT: &'static str = "https://token-c.example.test/token";

        fn new(scenario: SyntheticDiscoveryScenario) -> Self {
            Self {
                scenario,
                requests: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn metadata() -> serde_json::Value {
            serde_json::json!({
                "issuer": Self::ISSUER,
                "authorization_endpoint": "https://issuer-a.example.test/tenant/authorize",
                "token_endpoint": Self::TOKEN_ENDPOINT,
                "device_authorization_endpoint": Self::DEVICE_ENDPOINT,
                "registration_endpoint": "https://issuer-a.example.test/tenant/register",
                "grant_types_supported": [
                    "urn:ietf:params:oauth:grant-type:device_code",
                    "refresh_token"
                ],
                "authorization_response_iss_parameter_supported": false
            })
        }

        fn response(&self, params: &HttpRequestParams) -> (u16, Vec<HttpHeader>, Vec<u8>) {
            self.requests
                .lock()
                .expect("synthetic request recorder lock should not be poisoned")
                .push(params.url.clone());
            let json_response = |status, value: serde_json::Value| {
                (status, Vec::new(), serde_json::to_vec(&value).expect("fixture JSON encodes"))
            };
            match params.url.as_str() {
                Self::RESOURCE_URL => (
                    401,
                    vec![HttpHeader {
                        name: "www-authenticate".to_string(),
                        value: "Bearer resource_metadata=\"https://resource-a.example.test/.well-known/oauth-protected-resource/mcp\"".to_string(),
                        value_env_var: None,
                    }],
                    Vec::new(),
                ),
                url if url.contains("/.well-known/oauth-protected-resource") => json_response(
                    200,
                    serde_json::json!({
                        "resource": Self::RESOURCE_URL,
                        "authorization_servers": [Self::ISSUER]
                    }),
                ),
                "https://issuer-a.example.test/.well-known/oauth-authorization-server/tenant" => {
                    match self.scenario {
                        SyntheticDiscoveryScenario::Direct => {
                            json_response(200, Self::metadata())
                        }
                        SyntheticDiscoveryScenario::OidcFallback => {
                            (503, Vec::new(), Vec::new())
                        }
                        SyntheticDiscoveryScenario::SameOriginRedirect => (
                            302,
                            vec![HttpHeader {
                                name: "location".to_string(),
                                value: "https://issuer-a.example.test/tenant/.well-known/openid-configuration".to_string(),
                                value_env_var: None,
                            }],
                            Vec::new(),
                        ),
                    }
                }
                "https://issuer-a.example.test/.well-known/openid-configuration/tenant" => {
                    (404, Vec::new(), Vec::new())
                }
                "https://issuer-a.example.test/tenant/.well-known/openid-configuration" => {
                    json_response(200, Self::metadata())
                }
                Self::DEVICE_ENDPOINT => json_response(
                    200,
                    serde_json::json!({
                        "device_code": "synthetic-device-code",
                        "user_code": "SYNTHETIC-CODE",
                        "verification_uri": "https://device-b.example.test/verify",
                        "expires_in": 30,
                        "interval": 0
                    }),
                ),
                Self::TOKEN_ENDPOINT => json_response(
                    200,
                    serde_json::json!({
                        "access_token": "synthetic-access-token",
                        "refresh_token": "synthetic-refresh-token",
                        "token_type": "Bearer",
                        "expires_in": 3600
                    }),
                ),
                _ => (404, Vec::new(), Vec::new()),
            }
        }

        fn requested_urls(&self) -> Vec<String> {
            self.requests
                .lock()
                .expect("synthetic request recorder lock should not be poisoned")
                .clone()
        }
    }

    impl HttpClient for SyntheticDeviceAuthHttpClient {
        fn http_request(
            &self,
            _params: HttpRequestParams,
        ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
            Box::pin(async {
                Err(ExecServerError::HttpRequest(
                    "synthetic OAuth fixture expects streaming requests".to_string(),
                ))
            })
        }

        fn http_request_stream(
            &self,
            params: HttpRequestParams,
        ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>>
        {
            Box::pin(async move {
                let (status, headers, body) = self.response(&params);
                Ok((
                    HttpRequestResponse {
                        status,
                        headers,
                        body: Vec::new().into(),
                    },
                    HttpResponseBodyStream::from_chunks(vec![body]),
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
                    async move { Json(metadata) }
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
            Arc::new(RecordingHttpClient::default()),
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
            Arc::new(RecordingHttpClient::default()),
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
            test_http_client().await,
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
            test_http_client().await,
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
        let _home = crate::oauth::test_support::TempCodexHome::new();
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
                test_http_client().await,
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
        }))
        .await;

        let discovery = discover_streamable_http_oauth(
            &server.url,
            /*http_headers*/ None,
            /*env_http_headers*/ None,
            test_http_client().await,
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                scopes_supported: Some(vec!["profile".to_string(), "email".to_string()]),
                callback_mode: McpOAuthCallbackMode::IssuerBound,
                device_authorization: None,
            }
        );
    }

    #[test]
    fn discovery_preserves_cross_origin_device_endpoints_without_callback_issuer_support() {
        let metadata: AuthorizationMetadata = serde_json::from_value(serde_json::json!({
            "issuer": "https://issuer.example/tenant",
            "authorization_endpoint": "https://issuer.example/tenant/authorize",
            "token_endpoint": "https://tokens.example/token",
            "device_authorization_endpoint": "https://devices.example/device",
            "registration_endpoint": "https://issuer.example/tenant/register",
            "grant_types_supported": ["urn:ietf:params:oauth:grant-type:device_code"],
            "authorization_response_iss_parameter_supported": false
        }))
        .expect("metadata should parse");
        let receipt = DeviceMetadataReceiptCollector::new_for_test();
        receipt.record_for_test(
            "https://issuer.example/.well-known/oauth-authorization-server/tenant",
            &metadata,
        );

        let trusted = verified_device_authorization_from_metadata(&metadata, true, &receipt)
            .expect("issuer-validated metadata should create a device capability");

        assert_eq!(trusted.issuer(), "https://issuer.example/tenant");
        assert_eq!(
            trusted.device_authorization_endpoint(),
            Some("https://devices.example/device")
        );
        assert_eq!(trusted.token_endpoint(), "https://tokens.example/token");
        assert_eq!(
            callback_mode(&metadata).expect("callback mode should parse"),
            McpOAuthCallbackMode::CallbackSpecific
        );
    }

    #[test]
    fn device_metadata_urls_round_trip_only_canonical_well_known_paths() {
        for (request_url, expected_issuer) in [
            (
                "https://issuer.example/.well-known/oauth-authorization-server",
                "https://issuer.example/",
            ),
            (
                "https://issuer.example/.well-known/oauth-authorization-server/tenant",
                "https://issuer.example/tenant",
            ),
            (
                "https://issuer.example/.well-known/openid-configuration",
                "https://issuer.example/",
            ),
            (
                "https://issuer.example/.well-known/openid-configuration/tenant",
                "https://issuer.example/tenant",
            ),
            (
                "https://issuer.example/tenant/.well-known/openid-configuration",
                "https://issuer.example/tenant",
            ),
        ] {
            assert_eq!(
                expected_issuer_for_metadata_url(&url::Url::parse(request_url).unwrap()),
                Some(expected_issuer.to_string()),
                "canonical metadata request should map to its issuer: {request_url}"
            );
        }

        for request_url in [
            "https://issuer.example/.well-known/oauth-authorization-server/",
            "https://issuer.example/.well-known/oauth-authorization-server/tenant/",
            "https://issuer.example/.well-known/openid-configuration/",
            "https://issuer.example/.well-known/openid-configuration/tenant/",
            "https://issuer.example/tenant//.well-known/openid-configuration",
            "https://issuer.example/tenant/.well-known/openid-configuration/",
        ] {
            assert_eq!(
                expected_issuer_for_metadata_url(&url::Url::parse(request_url).unwrap()),
                None,
                "noncanonical metadata request must not establish issuer provenance: {request_url}"
            );
        }
    }

    #[tokio::test]
    async fn physical_https_discovery_receipt_drives_device_login_and_rejects_redirects() {
        let _home = crate::oauth::test_support::TempCodexHome::new();
        let direct_client = Arc::new(SyntheticDeviceAuthHttpClient::new(
            SyntheticDiscoveryScenario::Direct,
        ));
        let direct = discover_streamable_http_oauth(
            SyntheticDeviceAuthHttpClient::RESOURCE_URL,
            None,
            None,
            direct_client.clone(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("synthetic HTTPS discovery should succeed")
        .expect("issuer should advertise OAuth");
        let authorization = direct
            .device_authorization
            .expect("selected physical HTTPS metadata should mint a capability");
        assert_eq!(authorization.issuer(), SyntheticDeviceAuthHttpClient::ISSUER);
        assert_eq!(
            authorization.device_authorization_endpoint(),
            Some(SyntheticDeviceAuthHttpClient::DEVICE_ENDPOINT)
        );
        assert_eq!(authorization.token_endpoint(), SyntheticDeviceAuthHttpClient::TOKEN_ENDPOINT);
        assert_eq!(direct.callback_mode, McpOAuthCallbackMode::CallbackSpecific);

        crate::perform_oauth_device_login::perform_oauth_device_login(
            "synthetic-mcp-server",
            SyntheticDeviceAuthHttpClient::RESOURCE_URL,
            &authorization,
            direct_client.clone(),
            OAuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
            None,
            None,
            &[],
            Some("synthetic-client"),
            None,
            |_| {},
        )
        .await
        .expect("verified cross-origin device and token endpoints should complete");
        let stored = crate::stored_oauth_credentials(
            "synthetic-mcp-server",
            SyntheticDeviceAuthHttpClient::RESOURCE_URL,
            OAuthCredentialsStoreMode::File,
            AuthKeyringBackendKind::Direct,
        )
        .expect("synthetic credentials should reopen")
        .expect("device login should persist credentials");
        assert_eq!(
            stored.issuer.as_deref(),
            Some(SyntheticDeviceAuthHttpClient::ISSUER)
        );
        assert_eq!(stored.client_id, "synthetic-client");
        assert_eq!(
            stored.token_response.0.refresh_token().unwrap().secret(),
            "synthetic-refresh-token"
        );
        let direct_urls = direct_client.requested_urls();
        assert!(direct_urls.contains(
            &"https://issuer-a.example.test/.well-known/oauth-authorization-server/tenant"
                .to_string()
        ));
        assert!(direct_urls.contains(&SyntheticDeviceAuthHttpClient::DEVICE_ENDPOINT.to_string()));
        assert!(direct_urls.contains(&SyntheticDeviceAuthHttpClient::TOKEN_ENDPOINT.to_string()));

        let fallback_receipt_client = Arc::new(SyntheticDeviceAuthHttpClient::new(
            SyntheticDiscoveryScenario::OidcFallback,
        ));
        let adapter = OAuthHttpClientAdapter::new_with_redirect_mode(
            fallback_receipt_client.clone(),
            HeaderMap::new(),
            SyntheticDeviceAuthHttpClient::RESOURCE_URL,
            false,
            StreamableHttpRedirectMode::Legacy,
        )
        .expect("synthetic HTTPS resource URL should configure the adapter");
        let receipt = adapter.enable_device_metadata_receipt();
        let mut manager = AuthorizationManager::new_with_oauth_http_client(
            SyntheticDeviceAuthHttpClient::RESOURCE_URL,
            Arc::new(adapter),
        )
        .await
        .expect("synthetic HTTPS authorization manager should initialize");
        manager.set_allow_missing_issuer(true);
        let resolution = manager
            .resolve_metadata()
            .await
            .expect("SDK should select the successful OIDC fallback metadata");
        let selected_response = receipt
            .take_matching(&resolution.metadata, resolution.source.is_discovered())
            .expect("selected fallback metadata must have one physical adapter receipt");
        assert_eq!(
            selected_response.request_url.as_str(),
            "https://issuer-a.example.test/tenant/.well-known/openid-configuration",
            "device trust must bind the actual successful OIDC response, not the 503 request"
        );

        let fallback_client = Arc::new(SyntheticDeviceAuthHttpClient::new(
            SyntheticDiscoveryScenario::OidcFallback,
        ));
        let fallback = discover_streamable_http_oauth(
            SyntheticDeviceAuthHttpClient::RESOURCE_URL,
            None,
            None,
            fallback_client.clone(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("503 OIDC fallback discovery should preserve browser login")
        .expect("fallback issuer should advertise OAuth");
        assert_eq!(
            fallback
                .device_authorization
                .as_ref()
                .map(VerifiedDeviceAuthorization::issuer),
            Some(SyntheticDeviceAuthHttpClient::ISSUER)
        );
        let fallback_urls = fallback_client.requested_urls();
        assert!(fallback_urls.contains(
            &"https://issuer-a.example.test/.well-known/oauth-authorization-server/tenant"
                .to_string()
        ));
        assert!(fallback_urls.contains(
            &"https://issuer-a.example.test/tenant/.well-known/openid-configuration".to_string()
        ));

        let redirect_client = Arc::new(SyntheticDeviceAuthHttpClient::new(
            SyntheticDiscoveryScenario::SameOriginRedirect,
        ));
        let redirected = discover_streamable_http_oauth(
            SyntheticDeviceAuthHttpClient::RESOURCE_URL,
            None,
            None,
            redirect_client.clone(),
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("same-origin redirect should retain ordinary OAuth discovery")
        .expect("redirected metadata should retain browser login support");
        assert_eq!(redirected.device_authorization, None);
        assert_eq!(redirected.callback_mode, McpOAuthCallbackMode::CallbackSpecific);
        assert!(redirect_client
            .requested_urls()
            .contains(&"https://issuer-a.example.test/tenant/.well-known/openid-configuration".to_string()));
    }

    #[test]
    fn device_capability_rejects_untrusted_or_ambiguous_metadata_receipts() {
        let metadata_json = serde_json::json!({
            "issuer": "https://issuer.example",
            "authorization_endpoint": "https://issuer.example/authorize",
            "token_endpoint": "https://issuer.example/token",
            "device_authorization_endpoint": "https://devices.example/device",
            "grant_types_supported": ["urn:ietf:params:oauth:grant-type:device_code"]
        });
        let metadata: AuthorizationMetadata = serde_json::from_value(metadata_json.clone())
            .expect("metadata should parse");
        for (request_url, discovered) in [
            (
                "https://issuer.example/.well-known/oauth-authorization-server",
                false,
            ),
            (
                "http://issuer.example/.well-known/oauth-authorization-server",
                true,
            ),
            ("https://issuer.example/.well-known/untrusted-provider", true),
        ] {
            let receipt = DeviceMetadataReceiptCollector::new_for_test();
            receipt.record_for_test(request_url, &metadata);
            assert!(verified_device_authorization_from_metadata(
                &metadata,
                discovered,
                &receipt
            )
            .is_none());
        }

        let mut missing_issuer = metadata_json.clone();
        missing_issuer["issuer"] = serde_json::Value::Null;
        let missing_issuer: AuthorizationMetadata = serde_json::from_value(missing_issuer)
            .expect("missing issuer metadata should parse");
        let receipt = DeviceMetadataReceiptCollector::new_for_test();
        receipt.record_for_test(
            "https://issuer.example/.well-known/oauth-authorization-server",
            &missing_issuer,
        );
        assert!(verified_device_authorization_from_metadata(&missing_issuer, true, &receipt)
            .is_none());

        let mut mismatched_issuer = metadata_json.clone();
        mismatched_issuer["issuer"] = serde_json::json!("https://other.example");
        let mismatched_issuer: AuthorizationMetadata = serde_json::from_value(mismatched_issuer)
            .expect("mismatched issuer metadata should parse");
        let receipt = DeviceMetadataReceiptCollector::new_for_test();
        receipt.record_for_test(
            "https://issuer.example/.well-known/oauth-authorization-server",
            &mismatched_issuer,
        );
        assert!(verified_device_authorization_from_metadata(&mismatched_issuer, true, &receipt)
            .is_none());

        let receipt = DeviceMetadataReceiptCollector::new_for_test();
        receipt.record_for_test(
            "https://issuer.example/.well-known/oauth-authorization-server",
            &metadata,
        );
        let mut changed_postimage = metadata.clone();
        changed_postimage.token_endpoint = "https://other.example/token".to_string();
        assert!(verified_device_authorization_from_metadata(
            &changed_postimage,
            true,
            &receipt
        )
        .is_none());

        let receipt = DeviceMetadataReceiptCollector::new_for_test();
        receipt.record_for_test(
            "https://issuer.example/.well-known/oauth-authorization-server",
            &metadata,
        );
        receipt.record_for_test(
            "https://issuer.example/.well-known/openid-configuration",
            &metadata,
        );
        assert!(verified_device_authorization_from_metadata(&metadata, true, &receipt).is_none());
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
            test_http_client().await,
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(
            discovery,
            StreamableHttpOAuthDiscovery {
                scopes_supported: None,
                callback_mode: McpOAuthCallbackMode::CallbackSpecific,
                device_authorization: None,
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
            test_http_client().await,
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
            test_http_client().await,
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("discovery should succeed")
        .expect("oauth support should be detected");

        assert_eq!(discovery.scopes_supported, None);
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
            test_http_client().await,
            OAuthDiscoveryTimeout::LOCAL,
            StreamableHttpRedirectMode::Legacy,
        )
        .await
        .expect("support check should succeed")
        .is_some();

        assert!(supported);
    }
}
