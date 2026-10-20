use crate::oauth_metadata::discover_metadata;
use std::collections::HashMap;
use std::string::String;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use codex_exec_server::HttpClient as ExecServerHttpClient;
use codex_exec_server::RouteAwareHttpClient;
use codex_http_client::HttpClientBuilder;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use oauth2::TokenResponse;
use reqwest::Url;
use reqwest::header::HeaderMap;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::AuthorizationSession;
use rmcp::transport::auth::AuthorizationMetadata;
use rmcp::transport::auth::AuthorizationRequest;
use rmcp::transport::auth::OAuthClientConfig;
use rmcp::transport::auth::OAuthHttpClient;
use rmcp::transport::auth::OAuthState;
use sha2::Digest;
use sha2::Sha256;
use tiny_http::Response;
use tiny_http::Server;
use tokio::sync::oneshot;
use tokio::time::timeout;
use urlencoding::decode;

use crate::StoredOAuthTokens;
use crate::WrappedOAuthTokenResponse;
use crate::oauth::compute_expires_at_millis;
use crate::oauth_http_client::OAuthHttpClientAdapter;
use crate::save_oauth_tokens_locked;
use crate::utils::build_default_headers;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;

const FIXED_API_CLAIM_SCOPE: &str = "ops:write";
const FIXED_API_CLAIM_ISSUER: &str = "https://auth.grantmatler.com/realms/agent-ops";
const FIXED_API_CLAIM_RESOURCE: &str = "https://ops-api.grantmatler.com";
const FIXED_API_CLAIM_CLIENT_ID: &str = "codex-runtime-proof-api-claim";
const FIXED_API_CLAIM_CALLBACK_PORT: u16 = 8765;
const FIXED_API_CLAIM_CALLBACK_URL: &str = "http://127.0.0.1:8765/callback";
/// Maximum wait for the fixed Ops API OAuth callback in either interaction mode.
pub const FIXED_API_CLAIM_TIMEOUT_SECS: u64 = 300;

struct OAuthHttpContext {
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    http_client: Arc<dyn ExecServerHttpClient>,
}

struct CallbackServerGuard {
    server: Arc<Server>,
}

impl Drop for CallbackServerGuard {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthProviderError {
    error: Option<String>,
    error_description: Option<String>,
}

impl OAuthProviderError {
    pub fn new(error: Option<String>, error_description: Option<String>) -> Self {
        Self {
            error,
            error_description,
        }
    }
}

impl std::fmt::Display for OAuthProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.error.as_deref(), self.error_description.as_deref()) {
            (Some(error), Some(error_description)) => {
                write!(f, "OAuth provider returned `{error}`: {error_description}")
            }
            (Some(error), None) => write!(f, "OAuth provider returned `{error}`"),
            (None, Some(error_description)) => write!(f, "OAuth error: {error_description}"),
            (None, None) => write!(f, "OAuth provider returned an error"),
        }
    }
}

impl std::error::Error for OAuthProviderError {}

/// A single access token returned by a fixed authorization-code flow.
///
/// The callback state and token response stay in memory. This path never writes
/// Codex's persistent MCP credential store and returns only the access token.
pub struct OAuthAccessToken {
    access_token: zeroize::Zeroizing<String>,
    expires_at: i64,
}

impl OAuthAccessToken {
    pub fn access_token(&self) -> &str {
        self.access_token.as_str()
    }

    pub fn expires_at(&self) -> i64 {
        self.expires_at
    }
}

/// Runs the fixed Ops API loopback authorization-code flow without storing
/// returned credentials in Codex's persistent MCP OAuth store.
///
/// The issuer, API resource, client ID, redirect, and `ops:write` scope are
/// fixed in source. The flow requires the exact Keycloak realm endpoints,
/// authorization-code plus PKCE S256, a loopback callback, and a bounded
/// Bearer token lifetime. Any refresh response stays in memory only and is
/// discarded when the flow returns.
pub async fn perform_fixed_ops_api_access_token_login(
    open_authorization_url: impl FnOnce(&str) -> Result<()>,
) -> Result<OAuthAccessToken> {
    perform_oauth_access_token_login(
        FIXED_API_CLAIM_ISSUER,
        FIXED_API_CLAIM_RESOURCE,
        FIXED_API_CLAIM_CLIENT_ID,
        &[FIXED_API_CLAIM_SCOPE.to_string()],
        FIXED_API_CLAIM_CALLBACK_PORT,
        FIXED_API_CLAIM_CALLBACK_URL,
        FIXED_API_CLAIM_TIMEOUT_SECS,
        FixedOAuthInteraction::Browser(Box::new(open_authorization_url)),
    )
    .await
}

/// Runs the same fixed Ops API OAuth flow using a manually supplied loopback callback URL.
///
/// The caller must keep the authorization URL and callback input on the invoking terminal.
/// The callback is checked against the fixed redirect and the authorization request's state
/// before the same PKCE verifier and token-response checks used by the browser callback path.
pub async fn perform_fixed_ops_api_access_token_login_with_callback_input(
    read_callback_url: impl FnOnce(&str) -> Result<String>,
) -> Result<OAuthAccessToken> {
    perform_oauth_access_token_login(
        FIXED_API_CLAIM_ISSUER,
        FIXED_API_CLAIM_RESOURCE,
        FIXED_API_CLAIM_CLIENT_ID,
        &[FIXED_API_CLAIM_SCOPE.to_string()],
        FIXED_API_CLAIM_CALLBACK_PORT,
        FIXED_API_CLAIM_CALLBACK_URL,
        FIXED_API_CLAIM_TIMEOUT_SECS,
        FixedOAuthInteraction::CallbackInput(Box::new(read_callback_url)),
    )
    .await
}

enum FixedOAuthInteraction<'a> {
    Browser(Box<dyn FnOnce(&str) -> Result<()> + 'a>),
    CallbackInput(Box<dyn FnOnce(&str) -> Result<String> + 'a>),
}

enum FixedOAuthCallbackSource<'a> {
    Browser {
        open_authorization_url: Box<dyn FnOnce(&str) -> Result<()> + 'a>,
        callback: oneshot::Receiver<CallbackResult>,
        redirect_uri: String,
        _guard: CallbackServerGuard,
    },
    CallbackInput {
        read_callback_url: Box<dyn FnOnce(&str) -> Result<String> + 'a>,
        redirect_uri: String,
    },
}

#[allow(clippy::too_many_arguments)]
async fn perform_oauth_access_token_login<'a>(
    issuer_url: &str,
    resource_url: &str,
    client_id: &str,
    requested_scopes: &[String],
    callback_port: u16,
    callback_url: &str,
    timeout_secs: u64,
    interaction: FixedOAuthInteraction<'a>,
) -> Result<OAuthAccessToken> {
    validate_fixed_oauth_inputs(
        issuer_url,
        resource_url,
        client_id,
        requested_scopes,
        callback_port,
        callback_url,
        timeout_secs,
    )?;

    let callback_id = callback_id_from_server_url(issuer_url)?;
    let callback_source = match interaction {
        FixedOAuthInteraction::Browser(open_authorization_url) => {
            let bind_addr = format!("127.0.0.1:{callback_port}");
            let server = Arc::new(
                Server::http(&bind_addr)
                    .map_err(|_| anyhow!("bind fixed OAuth callback listener"))?,
            );
            let guard = CallbackServerGuard {
                server: Arc::clone(&server),
            };
            let redirect_uri = resolve_redirect_uri(&server, Some(callback_url))?;
            let redirect_uri = append_callback_id_to_redirect_uri(&redirect_uri, &callback_id)?;
            let callback_path = callback_path_from_redirect_uri(&redirect_uri)?;
            let (tx, callback) = oneshot::channel();
            spawn_callback_server(server, tx, callback_path);
            FixedOAuthCallbackSource::Browser {
                open_authorization_url,
                callback,
                redirect_uri,
                _guard: guard,
            }
        }
        FixedOAuthInteraction::CallbackInput(read_callback_url) => {
            let redirect_uri = append_callback_id_to_redirect_uri(callback_url, &callback_id)?;
            FixedOAuthCallbackSource::CallbackInput {
                read_callback_url,
                redirect_uri,
            }
        }
    };
    let redirect_uri = match &callback_source {
        FixedOAuthCallbackSource::Browser { redirect_uri, .. } => redirect_uri.clone(),
        FixedOAuthCallbackSource::CallbackInput { redirect_uri, .. } => redirect_uri.clone(),
    };

    let http_client = HttpClientBuilder::new()
        .without_redirects()
        .without_retries()
        .without_request_logging()
        .connect_timeout(Duration::from_secs(3))
        .build_direct()
        .map_err(|_| anyhow!("build fixed OAuth HTTP client"))?;
    let oauth_http_client: Arc<dyn OAuthHttpClient> =
        Arc::new(OAuthHttpClientAdapter::new_direct_with_max_timeout(
            http_client,
            HeaderMap::new(),
            Duration::from_secs(10),
        ));

    let (mut oauth_state, authorization_url) = start_fixed_oauth_authorization(
        issuer_url,
        resource_url,
        client_id,
        requested_scopes,
        &redirect_uri,
        oauth_http_client,
    )
    .await?;

    let (callback, callback_guard) = match callback_source {
        FixedOAuthCallbackSource::Browser {
            open_authorization_url,
            callback,
            _guard,
        } => {
            open_authorization_url(&authorization_url)
                .map_err(|_| anyhow!("open fixed OAuth authorization in the browser"))?;
            let callback = timeout(Duration::from_secs(timeout_secs), callback)
                .await
                .map_err(|_| anyhow!("fixed OAuth callback timed out"))?
                .map_err(|_| anyhow!("fixed OAuth callback was cancelled"))?;
            (callback, Some(_guard))
        }
        FixedOAuthCallbackSource::CallbackInput {
            read_callback_url,
            redirect_uri,
        } => {
            let callback_url = read_callback_url(&authorization_url)?;
            (
                parse_pasted_oauth_callback(&callback_url, &redirect_uri, &authorization_url)?,
                None,
            )
        }
    };

    let result = async {
        let OauthCallbackResult {
            code,
            state: csrf_state,
            issuer,
        } = match callback {
            CallbackResult::Success(callback) => callback,
            CallbackResult::Error(_) => bail!("fixed OAuth authorization was rejected"),
        };

        oauth_state
            .handle_callback_with_issuer(&code, &csrf_state, issuer.as_deref())
            .await
            .map_err(|_| anyhow!("complete fixed OAuth authorization"))?;
        let (_, credentials) = oauth_state
            .get_credentials()
            .await
            .map_err(|_| anyhow!("read in-memory OAuth response"))?;
        let credentials =
            credentials.ok_or_else(|| anyhow!("OAuth response omitted credentials"))?;
        access_token_from_response(credentials, requested_scopes)
    }
    .await;

    drop(callback_guard);
    result
}

async fn start_fixed_oauth_authorization(
    issuer_url: &str,
    resource_url: &str,
    client_id: &str,
    requested_scopes: &[String],
    redirect_uri: &str,
    oauth_http_client: Arc<dyn OAuthHttpClient>,
) -> Result<(OAuthState, String)> {
    let discovery_manager = AuthorizationManager::new_with_oauth_http_client(
        issuer_url,
        Arc::clone(&oauth_http_client),
    )
    .await
    .map_err(|_| anyhow!("initialize fixed OAuth issuer discovery"))?;
    let metadata = discover_metadata(&discovery_manager)
        .await
        .map_err(|_| anyhow!("discover fixed OAuth issuer metadata"))?;
    validate_fixed_oauth_metadata(&metadata, issuer_url)?;

    let mut resource_manager =
        AuthorizationManager::new_with_oauth_http_client(resource_url, oauth_http_client)
            .await
            .map_err(|_| anyhow!("initialize fixed OAuth resource"))?;
    resource_manager.set_metadata(metadata);
    let authorization_url = fixed_authorization_url(
        &mut resource_manager,
        client_id,
        redirect_uri,
        requested_scopes,
    )
    .await?;
    let session = AuthorizationSession::for_scope_upgrade(
        resource_manager,
        authorization_url.clone(),
        redirect_uri,
    );
    Ok((OAuthState::Session(session), authorization_url))
}

async fn fixed_authorization_url(
    resource_manager: &mut AuthorizationManager,
    client_id: &str,
    redirect_uri: &str,
    requested_scopes: &[String],
) -> Result<String> {
    resource_manager
        .configure_client(
            OAuthClientConfig::new(client_id, redirect_uri).with_scopes(requested_scopes.to_vec()),
        )
        .map_err(|_| anyhow!("configure fixed OAuth client"))?;
    let scopes = requested_scopes
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    resource_manager
        .get_authorization_url(&scopes)
        .await
        .map_err(|_| anyhow!("build fixed OAuth authorization request"))
}

fn validate_fixed_oauth_inputs(
    issuer_url: &str,
    resource_url: &str,
    client_id: &str,
    requested_scopes: &[String],
    callback_port: u16,
    callback_url: &str,
    timeout_secs: u64,
) -> Result<()> {
    validate_canonical_https_url(issuer_url)?;
    validate_canonical_https_origin(resource_url)?;
    let callback =
        Url::parse(callback_url).map_err(|_| anyhow!("fixed OAuth callback URL is invalid"))?;
    if client_id.is_empty()
        || client_id.len() > 512
        || !client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._~-".contains(&byte))
        || requested_scopes.len() != 1
        || requested_scopes.first().map(String::as_str) != Some(FIXED_API_CLAIM_SCOPE)
        || callback_port == 0
        || callback.scheme() != "http"
        || callback.host_str() != Some("127.0.0.1")
        || callback.port() != Some(callback_port)
        || callback.path() != "/callback"
        || !callback.username().is_empty()
        || callback.password().is_some()
        || callback.query().is_some()
        || callback.fragment().is_some()
        || callback.as_str() != callback_url
        || !(1..=600).contains(&timeout_secs)
    {
        bail!("fixed OAuth request is outside the admitted loopback contract");
    }
    Ok(())
}

fn validate_canonical_https_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| anyhow!("fixed OAuth URL is invalid"))?;
    let matches_canonical_origin =
        url.path() == "/" && url.as_str().strip_suffix('/') == Some(value);
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.as_str() != value && !matches_canonical_origin)
    {
        bail!("fixed OAuth URL must be canonical HTTPS");
    }
    Ok(url)
}

fn validate_canonical_https_origin(value: &str) -> Result<Url> {
    let url = validate_canonical_https_url(value)?;
    if url.path() != "/" {
        bail!("fixed OAuth resource must be an HTTPS origin");
    }
    Ok(url)
}

fn validate_fixed_oauth_metadata(metadata: &AuthorizationMetadata, issuer_url: &str) -> Result<()> {
    let issuer = validate_canonical_https_url(issuer_url)?;
    if metadata.issuer.as_deref() != Some(issuer_url)
        || !metadata
            .response_types_supported
            .as_ref()
            .is_some_and(|values| values.iter().any(|value| value == "code"))
        || !metadata
            .code_challenge_methods_supported
            .as_ref()
            .is_some_and(|values| values.iter().any(|value| value == "S256"))
    {
        bail!("fixed OAuth metadata does not match the issuer and PKCE contract");
    }
    let expected_authorization_endpoint = format!(
        "{}/protocol/openid-connect/auth",
        issuer.as_str().trim_end_matches('/')
    );
    let expected_token_endpoint = format!(
        "{}/protocol/openid-connect/token",
        issuer.as_str().trim_end_matches('/')
    );
    if metadata.authorization_endpoint != expected_authorization_endpoint
        || metadata.token_endpoint != expected_token_endpoint
    {
        bail!("fixed OAuth endpoints do not match the selected Keycloak realm");
    }
    Ok(())
}

fn access_token_from_response(
    response: rmcp::transport::auth::OAuthTokenResponse,
    requested_scopes: &[String],
) -> Result<OAuthAccessToken> {
    if !response
        .token_type()
        .as_ref()
        .eq_ignore_ascii_case("bearer")
    {
        bail!("fixed OAuth response is not a bearer token");
    }
    let expires_in = response
        .expires_in()
        .filter(|duration| !duration.is_zero() && *duration <= Duration::from_secs(86_400))
        .ok_or_else(|| anyhow!("fixed OAuth response has no bounded expiry"))?;
    if let Some(scopes) = response.scopes() {
        let granted = scopes
            .iter()
            .map(std::convert::AsRef::as_ref)
            .collect::<Vec<_>>();
        if granted.len() != requested_scopes.len()
            || requested_scopes
                .iter()
                .any(|scope| !granted.contains(&scope.as_str()))
        {
            bail!("fixed OAuth response scope does not match the request");
        }
    }
    let access_token = response.access_token().secret();
    if access_token.is_empty()
        || access_token.len() > 12 * 1024
        || access_token.chars().any(char::is_control)
    {
        bail!("fixed OAuth access token is outside its bounded format");
    }
    let received_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| anyhow!("system clock is before the Unix epoch"))?;
    let expires_at = received_at
        .as_secs()
        .checked_add(expires_in.as_secs())
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| anyhow!("fixed OAuth expiry is outside the supported range"))?;
    Ok(OAuthAccessToken {
        access_token: zeroize::Zeroizing::new(access_token.to_string()),
        expires_at,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn perform_oauth_login(
    server_name: &str,
    server_url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    scopes: &[String],
    oauth_client_id: Option<&str>,
    oauth_resource: Option<&str>,
    callback_port: Option<u16>,
    callback_url: Option<&str>,
) -> Result<()> {
    perform_oauth_login_with_browser_output(
        server_name,
        server_url,
        store_mode,
        keyring_backend_kind,
        http_headers,
        env_http_headers,
        scopes,
        oauth_client_id,
        oauth_resource,
        callback_port,
        callback_url,
        /*emit_browser_url*/ true,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn perform_oauth_login_silent(
    server_name: &str,
    server_url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    scopes: &[String],
    oauth_client_id: Option<&str>,
    oauth_resource: Option<&str>,
    callback_port: Option<u16>,
    callback_url: Option<&str>,
) -> Result<()> {
    perform_oauth_login_with_browser_output(
        server_name,
        server_url,
        store_mode,
        keyring_backend_kind,
        http_headers,
        env_http_headers,
        scopes,
        oauth_client_id,
        oauth_resource,
        callback_port,
        callback_url,
        /*emit_browser_url*/ false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn perform_oauth_login_with_browser_output(
    server_name: &str,
    server_url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    scopes: &[String],
    oauth_client_id: Option<&str>,
    oauth_resource: Option<&str>,
    callback_port: Option<u16>,
    callback_url: Option<&str>,
    emit_browser_url: bool,
) -> Result<()> {
    let http_context = OAuthHttpContext {
        http_headers,
        env_http_headers,
        http_client: Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
            OutboundProxyPolicy::ReqwestDefault,
        ))),
    };
    OauthLoginFlow::new(
        server_name,
        server_url,
        store_mode,
        keyring_backend_kind,
        http_context,
        scopes,
        oauth_client_id,
        oauth_resource,
        /*launch_browser*/ true,
        callback_port,
        callback_url,
        /*timeout_secs*/ None,
    )
    .await?
    .finish(emit_browser_url)
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn perform_oauth_login_return_url(
    server_name: &str,
    server_url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    scopes: &[String],
    oauth_client_id: Option<&str>,
    oauth_resource: Option<&str>,
    timeout_secs: Option<i64>,
    callback_port: Option<u16>,
    callback_url: Option<&str>,
) -> Result<OauthLoginHandle> {
    perform_oauth_login_return_url_with_http_client(
        server_name,
        server_url,
        store_mode,
        keyring_backend_kind,
        http_headers,
        env_http_headers,
        scopes,
        oauth_client_id,
        oauth_resource,
        timeout_secs,
        callback_port,
        callback_url,
        Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
            OutboundProxyPolicy::ReqwestDefault,
        ))),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn perform_oauth_login_return_url_with_http_client(
    server_name: &str,
    server_url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    scopes: &[String],
    oauth_client_id: Option<&str>,
    oauth_resource: Option<&str>,
    timeout_secs: Option<i64>,
    callback_port: Option<u16>,
    callback_url: Option<&str>,
    http_client: Arc<dyn ExecServerHttpClient>,
) -> Result<OauthLoginHandle> {
    let http_context = OAuthHttpContext {
        http_headers,
        env_http_headers,
        http_client,
    };
    let flow = OauthLoginFlow::new(
        server_name,
        server_url,
        store_mode,
        keyring_backend_kind,
        http_context,
        scopes,
        oauth_client_id,
        oauth_resource,
        /*launch_browser*/ false,
        callback_port,
        callback_url,
        timeout_secs,
    )
    .await?;

    let authorization_url = flow.authorization_url();
    let completion = flow.spawn();

    Ok(OauthLoginHandle::new(authorization_url, completion))
}

fn spawn_callback_server(
    server: Arc<Server>,
    tx: oneshot::Sender<CallbackResult>,
    expected_callback_path: String,
) {
    tokio::task::spawn_blocking(move || {
        while let Ok(request) = server.recv() {
            let path = request.url().to_string();
            match parse_oauth_callback(&path, &expected_callback_path) {
                CallbackOutcome::Success(callback) => {
                    let response = Response::from_string(
                        "Authentication complete. You may close this window.",
                    );
                    if let Err(err) = request.respond(response) {
                        eprintln!("Failed to respond to OAuth callback: {err}");
                    }
                    if tx.send(CallbackResult::Success(callback)).is_err() {
                        eprintln!("OAuth callback receiver is no longer available");
                    }
                    break;
                }
                CallbackOutcome::Error(error) => {
                    let response = Response::from_string(error.to_string()).with_status_code(400);
                    if let Err(err) = request.respond(response) {
                        eprintln!("Failed to respond to OAuth callback: {err}");
                    }
                    if tx.send(CallbackResult::Error(error)).is_err() {
                        eprintln!("OAuth callback receiver is no longer available");
                    }
                    break;
                }
                CallbackOutcome::Invalid => {
                    let response =
                        Response::from_string("Invalid OAuth callback").with_status_code(400);
                    if let Err(err) = request.respond(response) {
                        eprintln!("Failed to respond to OAuth callback: {err}");
                    }
                }
            }
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OauthCallbackResult {
    code: String,
    state: String,
    issuer: Option<String>,
}

#[derive(Debug)]
enum CallbackResult {
    Success(OauthCallbackResult),
    Error(OAuthProviderError),
}

#[derive(Debug, PartialEq, Eq)]
enum CallbackOutcome {
    Success(OauthCallbackResult),
    Error(OAuthProviderError),
    Invalid,
}

fn parse_oauth_callback(path: &str, expected_callback_path: &str) -> CallbackOutcome {
    let Some((route, query)) = path.split_once('?') else {
        return CallbackOutcome::Invalid;
    };
    if route != expected_callback_path {
        return CallbackOutcome::Invalid;
    }

    let mut code = None;
    let mut state = None;
    let mut issuer = None;
    let mut error = None;
    let mut error_description = None;

    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let Ok(decoded) = decode(value) else {
            continue;
        };
        let decoded = decoded.into_owned();
        match key {
            "code" => code = Some(decoded),
            "state" => state = Some(decoded),
            "iss" => issuer = Some(decoded),
            "error" => error = Some(decoded),
            "error_description" => error_description = Some(decoded),
            _ => {}
        }
    }

    if let (Some(code), Some(state)) = (code, state) {
        return CallbackOutcome::Success(OauthCallbackResult {
            code,
            state,
            issuer,
        });
    }

    if error.is_some() || error_description.is_some() {
        return CallbackOutcome::Error(OAuthProviderError::new(error, error_description));
    }

    CallbackOutcome::Invalid
}

fn parse_pasted_oauth_callback(
    input: &str,
    expected_redirect_uri: &str,
    authorization_url: &str,
) -> Result<CallbackResult> {
    const MAX_CALLBACK_URL_BYTES: usize = 64 * 1024;

    let input = input.trim();
    if input.len() > MAX_CALLBACK_URL_BYTES {
        bail!("OAuth callback URL exceeds 64 KiB");
    }
    let callback = Url::parse(input).map_err(|_| anyhow!("invalid OAuth callback URL"))?;
    let expected = Url::parse(expected_redirect_uri)
        .map_err(|_| anyhow!("invalid fixed OAuth redirect URI"))?;
    let has_userinfo = callback
        .as_str()
        .split('/')
        .nth(2)
        .is_some_and(|authority| authority.contains('@'));
    if callback.scheme() != expected.scheme()
        || callback.host_str() != expected.host_str()
        || callback.port_or_known_default() != expected.port_or_known_default()
        || callback.path() != expected.path()
        || has_userinfo
        || !callback.username().is_empty()
        || callback.password().is_some()
        || callback.fragment().is_some()
        || callback.query().is_none()
        || expected.query().is_some()
        || expected.fragment().is_some()
    {
        bail!("OAuth callback URL does not match this login's fixed redirect");
    }

    let authorization =
        Url::parse(authorization_url).map_err(|_| anyhow!("invalid fixed OAuth request"))?;
    let states = authorization
        .query_pairs()
        .filter(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned())
        .collect::<Vec<_>>();
    let [expected_state] = states.as_slice() else {
        bail!("fixed OAuth request has no unique state");
    };
    if expected_state.is_empty() {
        bail!("fixed OAuth request has no unique state");
    }

    let mut parameters = HashMap::new();
    for (name, value) in callback.query_pairs() {
        if parameters
            .insert(name.into_owned(), value.into_owned())
            .is_some()
        {
            bail!("OAuth callback URL contains duplicate parameters");
        }
    }
    let state = parameters
        .get("state")
        .filter(|state| !state.is_empty())
        .ok_or_else(|| anyhow!("OAuth callback URL is missing state"))?;
    if state != expected_state {
        bail!("OAuth callback state does not match this login");
    }
    if parameters.get("code").is_some_and(String::is_empty) {
        bail!("OAuth callback URL is missing an authorization code");
    }
    if (parameters.contains_key("error") || parameters.contains_key("error_description"))
        && parameters.contains_key("code")
    {
        bail!("OAuth callback URL contains both a code and an error");
    }

    let callback_path_and_query = format!(
        "{}?{}",
        callback.path(),
        callback.query().unwrap_or_default()
    );
    match parse_oauth_callback(&callback_path_and_query, expected.path()) {
        CallbackOutcome::Success(callback) => Ok(CallbackResult::Success(callback)),
        CallbackOutcome::Error(_) => Ok(CallbackResult::Error(OAuthProviderError::new(
            /*error*/ None,
            /*error_description*/ None,
        ))),
        CallbackOutcome::Invalid => bail!("OAuth callback URL is invalid"),
    }
}

pub struct OauthLoginHandle {
    authorization_url: String,
    completion: oneshot::Receiver<Result<()>>,
}

impl OauthLoginHandle {
    fn new(authorization_url: String, completion: oneshot::Receiver<Result<()>>) -> Self {
        Self {
            authorization_url,
            completion,
        }
    }

    pub fn authorization_url(&self) -> &str {
        &self.authorization_url
    }

    pub fn into_parts(self) -> (String, oneshot::Receiver<Result<()>>) {
        (self.authorization_url, self.completion)
    }

    pub async fn wait(self) -> Result<()> {
        self.completion
            .await
            .map_err(|err| anyhow!("OAuth login task was cancelled: {err}"))?
    }
}

struct OauthLoginFlow {
    auth_url: String,
    oauth_state: OAuthState,
    rx: oneshot::Receiver<CallbackResult>,
    guard: CallbackServerGuard,
    server_name: String,
    server_url: String,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    launch_browser: bool,
    timeout: Duration,
}

fn resolve_callback_port(callback_port: Option<u16>) -> Result<Option<u16>> {
    if let Some(config_port) = callback_port {
        if config_port == 0 {
            bail!(
                "invalid MCP OAuth callback port `{config_port}`: port must be between 1 and 65535"
            );
        }
        return Ok(Some(config_port));
    }

    Ok(None)
}

fn local_redirect_uri(server: &Server) -> Result<String> {
    match server.server_addr() {
        tiny_http::ListenAddr::IP(std::net::SocketAddr::V4(addr)) => {
            let ip = addr.ip();
            let port = addr.port();
            Ok(format!("http://{ip}:{port}/callback"))
        }
        tiny_http::ListenAddr::IP(std::net::SocketAddr::V6(addr)) => {
            let ip = addr.ip();
            let port = addr.port();
            Ok(format!("http://[{ip}]:{port}/callback"))
        }
        #[cfg(not(target_os = "windows"))]
        _ => Err(anyhow!("unable to determine callback address")),
    }
}

fn resolve_redirect_uri(server: &Server, callback_url: Option<&str>) -> Result<String> {
    let Some(callback_url) = callback_url else {
        return local_redirect_uri(server);
    };
    Url::parse(callback_url)
        .with_context(|| format!("invalid MCP OAuth callback URL `{callback_url}`"))?;
    Ok(callback_url.to_string())
}

fn callback_id_from_server_url(server_url: &str) -> Result<String> {
    let mut parsed =
        Url::parse(server_url).with_context(|| format!("invalid MCP server URL `{server_url}`"))?;
    parsed
        .host_str()
        .ok_or_else(|| anyhow!("MCP server URL `{server_url}` must include a host"))?;
    parsed.set_fragment(None);

    let digest = Sha256::digest(parsed.as_str().as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(&digest[..9]))
}

fn append_callback_id_to_redirect_uri(redirect_uri: &str, callback_id: &str) -> Result<String> {
    let mut parsed = Url::parse(redirect_uri)
        .with_context(|| format!("invalid redirect URI `{redirect_uri}`"))?;
    let path = parsed.path();
    let new_path = if path.ends_with('/') {
        format!("{path}{callback_id}")
    } else {
        format!("{path}/{callback_id}")
    };
    parsed.set_path(&new_path);
    Ok(parsed.to_string())
}

fn callback_path_from_redirect_uri(redirect_uri: &str) -> Result<String> {
    let parsed = Url::parse(redirect_uri)
        .with_context(|| format!("invalid redirect URI `{redirect_uri}`"))?;
    Ok(parsed.path().to_string())
}

fn callback_bind_host(callback_url: Option<&str>) -> &'static str {
    let Some(callback_url) = callback_url else {
        return "127.0.0.1";
    };

    let Ok(parsed) = Url::parse(callback_url) else {
        return "127.0.0.1";
    };

    match parsed.host_str() {
        Some("localhost" | "127.0.0.1" | "::1") | None => "127.0.0.1",
        Some(_) => "0.0.0.0",
    }
}

impl OauthLoginFlow {
    #[allow(clippy::too_many_arguments)]
    async fn new(
        server_name: &str,
        server_url: &str,
        store_mode: OAuthCredentialsStoreMode,
        keyring_backend_kind: AuthKeyringBackendKind,
        http_context: OAuthHttpContext,
        scopes: &[String],
        oauth_client_id: Option<&str>,
        oauth_resource: Option<&str>,
        launch_browser: bool,
        callback_port: Option<u16>,
        callback_url: Option<&str>,
        timeout_secs: Option<i64>,
    ) -> Result<Self> {
        const DEFAULT_OAUTH_TIMEOUT_SECS: i64 = 300;

        let bind_host = callback_bind_host(callback_url);
        let callback_port = resolve_callback_port(callback_port)?;
        let bind_addr = match callback_port {
            Some(port) => format!("{bind_host}:{port}"),
            None => format!("{bind_host}:0"),
        };

        let server = Arc::new(Server::http(&bind_addr).map_err(|err| anyhow!(err))?);
        let guard = CallbackServerGuard {
            server: Arc::clone(&server),
        };

        let redirect_uri = resolve_redirect_uri(&server, callback_url)?;
        let callback_id = callback_id_from_server_url(server_url)?;
        let redirect_uri = append_callback_id_to_redirect_uri(&redirect_uri, &callback_id)?;
        let callback_path = callback_path_from_redirect_uri(&redirect_uri)?;

        let (tx, rx) = oneshot::channel();
        spawn_callback_server(server, tx, callback_path);

        let OAuthHttpContext {
            http_headers,
            env_http_headers,
            http_client,
        } = http_context;
        let default_headers = build_default_headers(http_headers, env_http_headers)?;
        let oauth_http_client = Arc::new(OAuthHttpClientAdapter::new(http_client, default_headers));

        let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
        let oauth_state = start_authorization(
            server_url,
            oauth_http_client,
            &scope_refs,
            &redirect_uri,
            oauth_client_id,
        )
        .await?;
        let auth_url = append_query_param(
            &oauth_state.get_authorization_url().await?,
            "resource",
            oauth_resource,
        );
        let timeout_secs = timeout_secs.unwrap_or(DEFAULT_OAUTH_TIMEOUT_SECS).max(1);
        let timeout = Duration::from_secs(timeout_secs as u64);

        Ok(Self {
            auth_url,
            oauth_state,
            rx,
            guard,
            server_name: server_name.to_string(),
            server_url: server_url.to_string(),
            store_mode,
            keyring_backend_kind,
            launch_browser,
            timeout,
        })
    }

    fn authorization_url(&self) -> String {
        self.auth_url.clone()
    }

    async fn finish(mut self, emit_browser_url: bool) -> Result<()> {
        if self.launch_browser {
            let server_name = &self.server_name;
            let auth_url = &self.auth_url;
            if emit_browser_url {
                println!(
                    "Authorize `{server_name}` by opening this URL in your browser:\n{auth_url}\n"
                );
            }

            if webbrowser::open(auth_url).is_err() {
                if !emit_browser_url {
                    eprintln!(
                        "Authorize `{server_name}` by opening this URL in your browser:\n{auth_url}\n"
                    );
                }
                eprintln!("(Browser launch failed; please copy the URL above manually.)");
            }
        }

        let result = async {
            let callback = timeout(self.timeout, &mut self.rx)
                .await
                .context("timed out waiting for OAuth callback")?
                .context("OAuth callback was cancelled")?;
            let OauthCallbackResult {
                code,
                state: csrf_state,
                ..
            } = match callback {
                CallbackResult::Success(callback) => callback,
                CallbackResult::Error(error) => return Err(anyhow!(error)),
            };

            self.oauth_state
                .handle_callback(&code, &csrf_state)
                .await
                .context("failed to handle OAuth callback")?;

            let (client_id, credentials_opt) = self
                .oauth_state
                .get_credentials()
                .await
                .context("failed to retrieve OAuth credentials")?;
            let credentials = credentials_opt
                .ok_or_else(|| anyhow!("OAuth provider did not return credentials"))?;

            let expires_at = compute_expires_at_millis(&credentials);
            let stored = StoredOAuthTokens {
                server_name: self.server_name.clone(),
                url: self.server_url.clone(),
                client_id,
                token_response: WrappedOAuthTokenResponse(credentials),
                expires_at,
            };
            save_oauth_tokens_locked(
                &self.server_name,
                &stored,
                self.store_mode,
                self.keyring_backend_kind,
            )
            .await?;

            Ok(())
        }
        .await;

        drop(self.guard);
        result
    }

    fn spawn(self) -> oneshot::Receiver<Result<()>> {
        let server_name_for_logging = self.server_name.clone();
        let (tx, rx) = oneshot::channel();

        tokio::spawn(async move {
            let result = self.finish(/*emit_browser_url*/ false).await;

            if let Err(err) = &result {
                eprintln!(
                    "Failed to complete OAuth login for '{server_name_for_logging}': {err:#}"
                );
            }

            let _ = tx.send(result);
        });

        rx
    }
}

async fn start_authorization(
    server_url: &str,
    http_client: Arc<dyn OAuthHttpClient>,
    scopes: &[&str],
    redirect_uri: &str,
    oauth_client_id: Option<&str>,
) -> Result<OAuthState> {
    let mut auth_manager =
        AuthorizationManager::new_with_oauth_http_client(server_url, http_client).await?;
    let metadata = discover_metadata(&auth_manager).await?;
    auth_manager.set_metadata(metadata);

    let Some(oauth_client_id) = oauth_client_id.filter(|client_id| !client_id.trim().is_empty())
    else {
        let request = AuthorizationRequest::new(redirect_uri)
            .with_scopes(scopes.iter().copied())
            .with_client_name("Codex");
        let session = AuthorizationSession::new(auth_manager, request)
            .await
            .map_err(|(_, error)| error)?;
        return Ok(OAuthState::Session(session));
    };

    auth_manager.configure_client(
        OAuthClientConfig::new(oauth_client_id, redirect_uri)
            .with_scopes(scopes.iter().map(|scope| (*scope).to_string()).collect()),
    )?;
    let auth_url = auth_manager.get_authorization_url(scopes).await?;

    Ok(OAuthState::Session(
        AuthorizationSession::for_scope_upgrade(auth_manager, auth_url, redirect_uri),
    ))
}

fn append_query_param(url: &str, key: &str, value: Option<&str>) -> String {
    let Some(value) = value else {
        return url.to_string();
    };
    let value = value.trim();
    if value.is_empty() {
        return url.to_string();
    }
    if let Ok(mut parsed) = Url::parse(url) {
        parsed.query_pairs_mut().append_pair(key, value);
        return parsed.to_string();
    }
    let encoded = urlencoding::encode(value);
    let separator = if url.contains('?') { "&" } else { "?" };
    format!("{url}{separator}{key}={encoded}")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Json;
    use axum::Router;
    use axum::routing::get;
    use codex_exec_server::RouteAwareHttpClient;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use pretty_assertions::assert_eq;
    use reqwest::Url;
    use reqwest::header::HeaderMap;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::CallbackOutcome;
    use super::FIXED_API_CLAIM_CALLBACK_URL;
    use super::FIXED_API_CLAIM_CLIENT_ID;
    use super::FIXED_API_CLAIM_ISSUER;
    use super::FIXED_API_CLAIM_RESOURCE;
    use super::FIXED_API_CLAIM_SCOPE;
    use super::HttpClientBuilder;
    use super::OAuthHttpClientAdapter;
    use super::OAuthProviderError;
    use super::OauthCallbackResult;
    use super::access_token_from_response;
    use super::append_callback_id_to_redirect_uri;
    use super::append_query_param;
    use super::callback_id_from_server_url;
    use super::callback_path_from_redirect_uri;
    use super::fixed_authorization_url;
    use super::parse_oauth_callback;
    use super::parse_pasted_oauth_callback;
    use super::start_authorization;
    use super::validate_canonical_https_origin;
    use super::validate_fixed_oauth_inputs;
    use super::validate_fixed_oauth_metadata;

    async fn spawn_oauth_metadata_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind metadata listener");
        let addr = listener.local_addr().expect("read metadata listener addr");
        let base_url = format!("http://{addr}");
        let metadata = json!({
            "issuer": base_url,
            "authorization_endpoint": format!("{base_url}/oauth/authorize"),
            "token_endpoint": format!("{base_url}/oauth/token"),
            "scopes_supported": [""],
        });
        let mut path_scoped_metadata = metadata.clone();
        path_scoped_metadata["issuer"] = json!(format!("{base_url}/mcp"));
        let app = Router::new()
            .route(
                "/.well-known/oauth-authorization-server/mcp",
                get(move || {
                    let metadata = path_scoped_metadata.clone();
                    async move { Json(metadata) }
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(move || {
                    let metadata = metadata.clone();
                    async move { Json(metadata) }
                }),
            );

        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve oauth metadata");
        });

        base_url
    }

    #[tokio::test]
    async fn start_authorization_requires_published_metadata_for_both_client_id_paths() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let unexpected_requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = unexpected_requests.clone();
        let app = Router::new().fallback(move |request: axum::extract::Request| {
            let observed = observed.clone();
            async move {
                if request.method() != axum::http::Method::GET
                    || ["/authorize", "/token", "/register"].contains(&request.uri().path())
                {
                    observed.lock().unwrap().push((
                        request.method().to_string(),
                        request.uri().path().to_string(),
                    ));
                }
                axum::http::StatusCode::NOT_FOUND
            }
        });
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        for client_id in [None, Some("configured-client")] {
            let result = start_authorization(
                &format!("{base_url}/mcp"),
                Arc::new(OAuthHttpClientAdapter::new(
                    Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
                        OutboundProxyPolicy::ReqwestDefault,
                    ))),
                    HeaderMap::new(),
                )),
                &[],
                "http://127.0.0.1/callback",
                client_id,
            )
            .await;
            let error = match result {
                Ok(_) => panic!("unpublished OAuth metadata must be refused"),
                Err(error) => error,
            };
            assert!(matches!(
                error.downcast_ref::<rmcp::transport::auth::AuthError>(),
                Some(rmcp::transport::auth::AuthError::NoAuthorizationSupport)
            ));
        }
        server.abort();
        assert_eq!(
            *unexpected_requests.lock().unwrap(),
            Vec::<(String, String)>::new()
        );
    }

    #[tokio::test]
    async fn start_authorization_uses_configured_client_id() {
        let base_url = spawn_oauth_metadata_server().await;
        let oauth_state = start_authorization(
            &format!("{base_url}/mcp"),
            Arc::new(OAuthHttpClientAdapter::new(
                Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
                    OutboundProxyPolicy::ReqwestDefault,
                ))),
                HeaderMap::new(),
            )),
            &[],
            "http://127.0.0.1/callback",
            Some("eci-prd-pub-codex-123"),
        )
        .await
        .expect("start oauth authorization");

        let authorization_url = oauth_state
            .get_authorization_url()
            .await
            .expect("read authorization url");
        let auth_url = Url::parse(&authorization_url).expect("authorization url should parse");
        let client_id = auth_url
            .query_pairs()
            .find(|(key, _)| key == "client_id")
            .map(|(_, value)| value.into_owned());

        assert_eq!(client_id.as_deref(), Some("eci-prd-pub-codex-123"));
    }

    #[tokio::test]
    async fn fixed_access_token_request_binds_client_resource_and_pkce_scope() {
        let resource = FIXED_API_CLAIM_RESOURCE;
        let expected_resource = Url::parse(resource).expect("parse fixed OAuth resource");
        let redirect = append_callback_id_to_redirect_uri(
            FIXED_API_CLAIM_CALLBACK_URL,
            &callback_id_from_server_url(FIXED_API_CLAIM_ISSUER)
                .expect("derive the fixed callback path"),
        )
        .expect("append the fixed callback identifier");
        assert_eq!(redirect, "http://127.0.0.1:8765/callback/gGLT7yyzxEu8");
        let http_client = HttpClientBuilder::new()
            .without_redirects()
            .without_retries()
            .without_request_logging()
            .connect_timeout(Duration::from_secs(3))
            .build_direct()
            .expect("build fixed OAuth HTTP client");
        let mut manager = rmcp::transport::AuthorizationManager::new_with_oauth_http_client(
            resource,
            Arc::new(OAuthHttpClientAdapter::new_direct_with_max_timeout(
                http_client,
                HeaderMap::new(),
                Duration::from_secs(10),
            )),
        )
        .await
        .expect("create fixed resource authorization manager");
        let metadata: rmcp::transport::auth::AuthorizationMetadata =
            serde_json::from_value(json!({
                "issuer": FIXED_API_CLAIM_ISSUER,
                "authorization_endpoint": "https://auth.grantmatler.com/realms/agent-ops/protocol/openid-connect/auth",
                "token_endpoint": "https://auth.grantmatler.com/realms/agent-ops/protocol/openid-connect/token",
                "response_types_supported": ["code"],
                "code_challenge_methods_supported": ["S256"]
            }))
            .expect("parse fixed OAuth metadata");
        validate_fixed_oauth_metadata(&metadata, FIXED_API_CLAIM_ISSUER)
            .expect("metadata matches the fixed issuer");
        manager.set_metadata(metadata);
        let authorization_url = fixed_authorization_url(
            &mut manager,
            FIXED_API_CLAIM_CLIENT_ID,
            &redirect,
            &[FIXED_API_CLAIM_SCOPE.to_string()],
        )
        .await
        .expect("build fixed authorization URL");

        let authorization_url = Url::parse(&authorization_url).expect("parse authorization URL");
        let query = authorization_url.query_pairs().collect::<Vec<_>>();
        let value = |key: &str| {
            query
                .iter()
                .find(|(candidate, _)| candidate == key)
                .map(|(_, value)| value.as_ref())
        };
        assert_eq!(value("client_id"), Some(FIXED_API_CLAIM_CLIENT_ID));
        assert_eq!(value("redirect_uri"), Some(redirect.as_str()));
        assert_eq!(value("response_type"), Some("code"));
        assert_eq!(value("resource"), Some(expected_resource.as_str()));
        assert_eq!(value("scope"), Some(FIXED_API_CLAIM_SCOPE));
        assert_eq!(value("code_challenge_method"), Some("S256"));
        assert_eq!(value("state").map(str::len), Some(22));
        assert_eq!(value("code_challenge").map(str::len), Some(43));
        assert!(!query.iter().any(|(key, value)| {
            key == "scope"
                && value
                    .split_whitespace()
                    .any(|scope| scope == "offline_access")
        }));
    }

    #[test]
    fn fixed_access_token_flow_rejects_unbounded_or_non_loopback_inputs() {
        let scope = vec![FIXED_API_CLAIM_SCOPE.to_string()];
        assert!(
            validate_fixed_oauth_inputs(
                FIXED_API_CLAIM_ISSUER,
                FIXED_API_CLAIM_RESOURCE,
                FIXED_API_CLAIM_CLIENT_ID,
                &scope,
                /*callback_port*/ 8765,
                "http://127.0.0.1:8765/callback",
                /*timeout_secs*/ 300,
            )
            .is_ok()
        );
        for callback in [
            "http://localhost:8765/callback",
            "http://127.0.0.1:8765/callback?next=https://example.com",
            "https://127.0.0.1:8765/callback",
        ] {
            assert!(
                validate_fixed_oauth_inputs(
                    FIXED_API_CLAIM_ISSUER,
                    FIXED_API_CLAIM_RESOURCE,
                    FIXED_API_CLAIM_CLIENT_ID,
                    &scope,
                    /*callback_port*/ 8765,
                    callback,
                    /*timeout_secs*/ 300,
                )
                .is_err()
            );
        }
        assert!(
            validate_fixed_oauth_inputs(
                FIXED_API_CLAIM_ISSUER,
                FIXED_API_CLAIM_RESOURCE,
                FIXED_API_CLAIM_CLIENT_ID,
                &["ops:write offline_access".to_string()],
                /*callback_port*/ 8765,
                "http://127.0.0.1:8765/callback",
                /*timeout_secs*/ 300,
            )
            .is_err()
        );
    }

    #[test]
    fn canonical_https_accepts_the_fixed_bare_api_origin_only() {
        assert!(validate_canonical_https_origin(FIXED_API_CLAIM_RESOURCE).is_ok());
        assert!(validate_canonical_https_origin("https://ops-api.grantmatler.com/").is_ok());
        for value in [
            "https://ops-api.grantmatler.com:443",
            "https://OPS-API.grantmatler.com",
            "https://ops-api.grantmatler.com/path",
            "https://ops-api.grantmatler.com?query=value",
        ] {
            assert!(validate_canonical_https_origin(value).is_err());
        }
    }

    #[test]
    fn fixed_access_token_flow_rejects_wrong_issuer_endpoint_or_missing_pkce() {
        let valid = json!({
            "issuer": "https://auth.grantmatler.com/realms/agent-ops",
            "authorization_endpoint": "https://auth.grantmatler.com/realms/agent-ops/protocol/openid-connect/auth",
            "token_endpoint": "https://auth.grantmatler.com/realms/agent-ops/protocol/openid-connect/token",
            "response_types_supported": ["code"],
            "code_challenge_methods_supported": ["S256"]
        });
        for (key, value) in [
            ("issuer", json!("https://auth.grantmatler.com/other")),
            ("token_endpoint", json!("https://other.example/token")),
            (
                "authorization_endpoint",
                json!("https://auth.grantmatler.com/other/authorize"),
            ),
            ("code_challenge_methods_supported", json!(["plain"])),
        ] {
            let mut metadata = valid.clone();
            metadata[key] = value;
            let metadata: rmcp::transport::auth::AuthorizationMetadata =
                serde_json::from_value(metadata).expect("parse adversarial metadata");
            assert!(
                validate_fixed_oauth_metadata(
                    &metadata,
                    "https://auth.grantmatler.com/realms/agent-ops"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn fixed_access_token_extracts_only_bounded_bearer_and_drops_refresh_response() {
        let response: rmcp::transport::auth::OAuthTokenResponse = serde_json::from_value(json!({
            "access_token": "bounded-test-token",
            "token_type": "Bearer",
            "expires_in": 300,
            "scope": "ops:write",
            "refresh_token": "must-not-be-returned-or-persisted"
        }))
        .expect("parse OAuth token response");
        let token = access_token_from_response(response, &[FIXED_API_CLAIM_SCOPE.to_string()])
            .expect("extract the access token only");
        assert_eq!(token.access_token(), "bounded-test-token");
        assert!(token.expires_at() > 0);

        let response = serde_json::from_value(json!({
            "access_token": "bounded-test-token",
            "token_type": "Bearer",
            "expires_in": 300,
            "scope": "ops:write offline_access",
            "refresh_token": "ignored"
        }))
        .expect("parse extra-scope OAuth response");
        assert!(
            access_token_from_response(response, &[FIXED_API_CLAIM_SCOPE.to_string()]).is_err()
        );
    }

    #[test]
    fn fixed_access_token_rejects_wrong_type_and_unbounded_lifetime() {
        for (token_type, expires_in) in [("MAC", 300), ("Bearer", 0), ("Bearer", 86_401)] {
            let response = serde_json::from_value(json!({
                "access_token": "bounded-test-token",
                "token_type": token_type,
                "expires_in": expires_in,
                "scope": "ops:write"
            }))
            .expect("parse OAuth token response");
            assert!(
                access_token_from_response(response, &[FIXED_API_CLAIM_SCOPE.to_string()]).is_err()
            );
        }
    }

    #[test]
    fn parse_oauth_callback_accepts_default_path() {
        let parsed = parse_oauth_callback("/callback?code=abc&state=xyz", "/callback");
        assert!(matches!(parsed, CallbackOutcome::Success(_)));
    }

    #[test]
    fn parse_oauth_callback_accepts_custom_path() {
        let parsed = parse_oauth_callback("/oauth/callback?code=abc&state=xyz", "/oauth/callback");
        assert!(matches!(parsed, CallbackOutcome::Success(_)));
    }

    #[test]
    fn parse_oauth_callback_accepts_callback_id_path() {
        let parsed =
            parse_oauth_callback("/callback/abc123?code=abc&state=xyz", "/callback/abc123");
        assert!(matches!(parsed, CallbackOutcome::Success(_)));
    }

    #[test]
    fn parse_oauth_callback_preserves_authorization_response_issuer() {
        let parsed = parse_oauth_callback(
            "/callback?code=abc&state=xyz&iss=https%3A%2F%2Fissuer.example",
            "/callback",
        );
        assert!(matches!(
            parsed,
            CallbackOutcome::Success(OauthCallbackResult {
                issuer: Some(issuer),
                ..
            }) if issuer == "https://issuer.example"
        ));
    }

    #[test]
    fn parse_oauth_callback_rejects_missing_callback_id_path() {
        let parsed = parse_oauth_callback("/callback?code=abc&state=xyz", "/callback/abc123");
        assert!(matches!(parsed, CallbackOutcome::Invalid));
    }

    #[test]
    fn parse_oauth_callback_rejects_wrong_path() {
        let parsed = parse_oauth_callback("/callback?code=abc&state=xyz", "/oauth/callback");
        assert!(matches!(parsed, CallbackOutcome::Invalid));
    }

    #[test]
    fn pasted_fixed_callback_requires_the_registered_redirect_and_matching_state() {
        let redirect = "http://127.0.0.1:8765/callback/gGLT7yyzxEu8";
        let authorization =
            "https://auth.grantmatler.com/authorize?state=expected-state&code_challenge=challenge";
        let callback = parse_pasted_oauth_callback(
            " http://127.0.0.1:8765/callback/gGLT7yyzxEu8?code=code%2Bvalue&state=expected-state&iss=https%3A%2F%2Fauth.grantmatler.com&session_state=opaque ",
            redirect,
            authorization,
        )
        .expect("parse the fixed callback URL");
        assert!(matches!(
            callback,
            CallbackResult::Success(OauthCallbackResult {
                code,
                state,
                issuer: Some(issuer),
            }) if code == "code+value"
                && state == "expected-state"
                && issuer == "https://auth.grantmatler.com"
        ));
    }

    #[test]
    fn pasted_fixed_callback_rejects_unbound_or_ambiguous_input_without_echoing_it() {
        let redirect = "http://127.0.0.1:8765/callback/gGLT7yyzxEu8";
        let authorization =
            "https://auth.grantmatler.com/authorize?state=expected-state&code_challenge=challenge";
        for callback in [
            "http://127.0.0.2:8765/callback/gGLT7yyzxEu8?code=secret-callback-code&state=expected-state",
            "http://127.0.0.1:8766/callback/gGLT7yyzxEu8?code=secret-callback-code&state=expected-state",
            "http://127.0.0.1:8765/callback/other?code=secret-callback-code&state=expected-state",
            "https://127.0.0.1:8765/callback/gGLT7yyzxEu8?code=secret-callback-code&state=expected-state",
            "http://user@127.0.0.1:8765/callback/gGLT7yyzxEu8?code=secret-callback-code&state=expected-state",
            "http://127.0.0.1:8765/callback/gGLT7yyzxEu8?code=secret-callback-code&state=expected-state#fragment",
            "http://@127.0.0.1:8765/callback/gGLT7yyzxEu8?code=secret-callback-code&state=expected-state",
            "http://127.0.0.1:8765/callback/gGLT7yyzxEu8?code=&state=expected-state",
            "http://127.0.0.1:8765/callback/gGLT7yyzxEu8?code=secret-callback-code&state=wrong-state",
            "http://127.0.0.1:8765/callback/gGLT7yyzxEu8?code=secret-callback-code&state=expected-state&state=second",
            "http://127.0.0.1:8765/callback/gGLT7yyzxEu8?code=secret-callback-code&%63ode=second&state=expected-state",
            "http://127.0.0.1:8765/callback/gGLT7yyzxEu8?code=secret-callback-code&state=expected-state&error=denied",
        ] {
            let error = parse_pasted_oauth_callback(callback, redirect, authorization)
                .expect_err("reject callback not bound to this login");
            assert!(!format!("{error:#}").contains("secret-callback-code"));
        }
        assert!(parse_pasted_oauth_callback(
            &"x".repeat(/*n*/ 65_537),
            redirect,
            authorization,
        )
        .is_err());
    }

    #[test]
    fn pasted_provider_error_is_state_bound_and_redacted() {
        let redirect = "http://127.0.0.1:8765/callback/gGLT7yyzxEu8";
        let authorization =
            "https://auth.grantmatler.com/authorize?state=expected-state&code_challenge=challenge";
        let callback = parse_pasted_oauth_callback(
            "http://127.0.0.1:8765/callback/gGLT7yyzxEu8?error=access_denied&error_description=secret-description&state=expected-state",
            redirect,
            authorization,
        )
        .expect("parse state-bound provider error");
        let CallbackResult::Error(error) = callback else {
            panic!("expected provider error");
        };
        assert_eq!(error.to_string(), "OAuth provider returned an error");
        assert!(parse_pasted_oauth_callback(
            "http://127.0.0.1:8765/callback/gGLT7yyzxEu8?error=access_denied&state=wrong-state",
            redirect,
            authorization,
        )
        .is_err());
    }

    #[test]
    fn parse_oauth_callback_returns_provider_error() {
        let parsed = parse_oauth_callback(
            "/callback?error=invalid_scope&error_description=scope%20rejected",
            "/callback",
        );

        assert_eq!(
            parsed,
            CallbackOutcome::Error(OAuthProviderError::new(
                Some("invalid_scope".to_string()),
                Some("scope rejected".to_string()),
            ))
        );
    }

    #[test]
    fn callback_path_comes_from_redirect_uri() {
        let path = callback_path_from_redirect_uri("https://example.com/oauth/callback")
            .expect("redirect URI should parse");
        assert_eq!(path, "/oauth/callback");
    }

    #[test]
    fn callback_id_is_bound_to_server_url() {
        let callback_id = callback_id_from_server_url("https://mcp.example.com/mcp?tenant=one")
            .expect("server URL should parse");
        let same_without_fragment =
            callback_id_from_server_url("https://mcp.example.com/mcp?tenant=one#unused")
                .expect("server URL should parse");
        let different_path = callback_id_from_server_url("https://mcp.example.com/sse?tenant=one")
            .expect("server URL should parse");
        let different_query = callback_id_from_server_url("https://mcp.example.com/mcp?tenant=two")
            .expect("server URL should parse");
        let different_origin = callback_id_from_server_url("https://mcp.example.com:8443/mcp")
            .expect("server URL should parse");

        assert_eq!(callback_id, same_without_fragment);
        assert_ne!(callback_id, different_path);
        assert_ne!(callback_id, different_query);
        assert_ne!(callback_id, different_origin);
        assert_eq!(callback_id.len(), 12);
        assert!(
            callback_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        );
    }

    #[test]
    fn callback_id_is_appended_to_redirect_uri_path() {
        let redirect_uri =
            append_callback_id_to_redirect_uri("http://127.0.0.1:1234/callback", "abc123")
                .expect("redirect URI should parse");

        assert_eq!(redirect_uri, "http://127.0.0.1:1234/callback/abc123");
    }

    #[test]
    fn callback_id_is_appended_before_redirect_uri_query() {
        let redirect_uri = append_callback_id_to_redirect_uri(
            "https://callbacks.example.com/oauth/callback?provider=github",
            "abc123",
        )
        .expect("redirect URI should parse");

        assert_eq!(
            redirect_uri,
            "https://callbacks.example.com/oauth/callback/abc123?provider=github"
        );
    }

    #[test]
    fn append_query_param_adds_resource_to_absolute_url() {
        let url = append_query_param(
            "https://example.com/authorize?scope=read",
            "resource",
            Some("https://api.example.com"),
        );

        assert_eq!(
            url,
            "https://example.com/authorize?scope=read&resource=https%3A%2F%2Fapi.example.com"
        );
    }

    #[test]
    fn append_query_param_ignores_empty_values() {
        let url = append_query_param(
            "https://example.com/authorize?scope=read",
            "resource",
            Some("   "),
        );

        assert_eq!(url, "https://example.com/authorize?scope=read");
    }

    #[test]
    fn append_query_param_handles_unparseable_url() {
        let url = append_query_param("not a url", "resource", Some("api/resource"));

        assert_eq!(url, "not a url?resource=api%2Fresource");
    }
}
