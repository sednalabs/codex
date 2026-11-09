//! Headless OAuth device grant for MCP servers.
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use codex_config::types::AuthKeyringBackendKind;
use codex_config::types::OAuthCredentialsStoreMode;
use codex_exec_server::HttpClient;
use oauth2::PkceCodeChallenge;
use rmcp::transport::auth::OAuthHttpRedirectPolicy;
use rmcp::transport::auth::OAuthTokenResponse;
use serde::Deserialize;
use serde::Serialize;
use tokio::time::Instant;
use url::Url;

use crate::device_oauth_metadata::discover_device_metadata;
use crate::http_client_adapter::StreamableHttpRedirectMode;
use crate::oauth::StoredOAuthTokens;
use crate::oauth::WrappedOAuthTokenResponse;
use crate::oauth::compute_expires_at_millis;
use crate::oauth::save_oauth_tokens;
use crate::oauth_http_client::OAuthHttpClientAdapter;
use crate::utils::build_default_headers;

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

pub struct DeviceAuthorizationPrompt {
    pub verification_uri: String,
    pub user_code: String,
}

#[allow(clippy::too_many_arguments)]
pub async fn perform_oauth_device_login(
    server_name: &str,
    server_url: &str,
    store_mode: OAuthCredentialsStoreMode,
    keyring_backend_kind: AuthKeyringBackendKind,
    http_headers: Option<HashMap<String, String>>,
    env_http_headers: Option<HashMap<String, String>>,
    scopes: &[String],
    use_metadata_scopes: bool,
    client_id: Option<&str>,
    resource: Option<&str>,
    http_client: Arc<dyn HttpClient>,
    mut show_prompt: impl FnMut(DeviceAuthorizationPrompt),
) -> Result<()> {
    let configured_headers = http_headers.as_ref().is_some_and(|h| !h.is_empty())
        || env_http_headers.as_ref().is_some_and(|h| !h.is_empty());
    let headers = build_default_headers(http_headers, env_http_headers)?;
    let client = Arc::new(
        OAuthHttpClientAdapter::new_with_max_timeout_and_redirect_mode(
            http_client,
            headers,
            server_url,
            Duration::from_secs(30),
            configured_headers,
            StreamableHttpRedirectMode::Legacy,
        )?,
    );
    let metadata = discover_device_metadata(server_url, client.clone()).await?;
    let discovered_scopes = if use_metadata_scopes && scopes.is_empty() {
        metadata.scopes_supported.as_ref().map(|values| {
            let mut normalized = Vec::new();
            for value in values {
                let value = value.trim();
                if !value.is_empty() && !normalized.iter().any(|existing| existing == value) {
                    normalized.push(value.to_string());
                }
            }
            normalized
        })
    } else {
        None
    };
    let scopes = discovered_scopes.as_deref().unwrap_or(scopes);
    let client_id = match client_id.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => id.to_string(),
        None => {
            let endpoint = metadata.registration_endpoint.as_deref().ok_or_else(|| {
                anyhow!("device login requires a public client ID or dynamic client registration")
            })?;
            register_device_client(&client, endpoint, scopes, &metadata.grant_types_supported)
                .await?
        }
    };
    // Refuse changed metadata before sending the device request or token polling.
    let current = discover_device_metadata(server_url, client.clone()).await?;
    if current.issuer != metadata.issuer
        || current.token_endpoint != metadata.token_endpoint
        || current.device_authorization_endpoint != metadata.device_authorization_endpoint
        || current.registration_endpoint != metadata.registration_endpoint
    {
        bail!("OAuth device authorization metadata changed during login");
    }
    let mut pkce = Some(DevicePkce::new());
    let details = match request_device_code(
        &client,
        &metadata.device_authorization_endpoint,
        &client_id,
        scopes,
        resource,
        pkce.as_ref(),
    )
    .await
    {
        Err(error)
            if error
                .downcast_ref::<ProviderError>()
                .is_some_and(|e| e.0 == "invalid_request") =>
        {
            pkce = None;
            request_device_code(
                &client,
                &metadata.device_authorization_endpoint,
                &client_id,
                scopes,
                resource,
                None,
            )
            .await?
        }
        result => result?,
    };
    prompt(&details, &mut show_prompt)?;
    let token = match poll_token(
        &client,
        &metadata.token_endpoint,
        &client_id,
        resource,
        &details,
        pkce.as_ref(),
    )
    .await
    {
        Err(error)
            if pkce.is_some()
                && error
                    .downcast_ref::<ProviderError>()
                    .is_some_and(|e| e.0 == "invalid_request") =>
        {
            let details = request_device_code(
                &client,
                &metadata.device_authorization_endpoint,
                &client_id,
                scopes,
                resource,
                None,
            )
            .await?;
            prompt(&details, &mut show_prompt)?;
            poll_token(
                &client,
                &metadata.token_endpoint,
                &client_id,
                resource,
                &details,
                None,
            )
            .await?
        }
        result => result?,
    };
    let expires_at = compute_expires_at_millis(&token);
    let stored = StoredOAuthTokens {
        server_name: server_name.to_string(),
        url: server_url.to_string(),
        issuer: Some(metadata.issuer),
        client_id,
        token_response: WrappedOAuthTokenResponse(token),
        expires_at,
    };
    save_oauth_tokens(server_name, &stored, store_mode, keyring_backend_kind).await
}

#[derive(Debug)]
struct ProviderError(String);
impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "OAuth device request was rejected ({})", self.0)
    }
}
impl std::error::Error for ProviderError {}

async fn post(
    client: &OAuthHttpClientAdapter,
    endpoint: &str,
    content_type: &str,
    body: Vec<u8>,
) -> Result<oauth2::HttpResponse> {
    let request = oauth2::http::Request::builder()
        .method("POST")
        .uri(endpoint)
        .header("content-type", content_type)
        .body(body)?;
    // Never follow redirects carrying a device code, verifier, or client metadata.
    client
        .execute_request(
            request,
            OAuthHttpRedirectPolicy::Stop,
            Some(Duration::from_secs(30)),
        )
        .await
        .map_err(|e| anyhow!("OAuth device HTTP request failed: {e}"))
}

fn form(items: &[(&str, String)]) -> Vec<u8> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in items {
        serializer.append_pair(name, value);
    }
    serializer.finish().into_bytes()
}

fn provider_error(response: &oauth2::HttpResponse) -> anyhow::Error {
    #[derive(Deserialize)]
    struct Body {
        error: String,
    }
    let error = serde_json::from_slice::<Body>(response.body())
        .ok()
        .map(|body| body.error)
        .filter(|s| {
            matches!(
                s.as_str(),
                "invalid_request"
                    | "authorization_pending"
                    | "slow_down"
                    | "expired_token"
                    | "access_denied"
                    | "invalid_scope"
                    | "invalid_client"
                    | "unauthorized_client"
                    | "unsupported_grant_type"
            )
        })
        .unwrap_or_else(|| format!("http_{}", response.status().as_u16()));
    anyhow::Error::new(ProviderError(error))
}

#[derive(Serialize)]
struct Registration<'a> {
    client_name: &'static str,
    grant_types: Vec<&'static str>,
    token_endpoint_auth_method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<&'a str>,
}
#[derive(Deserialize)]
struct Registered {
    client_id: String,
}
async fn register_device_client(
    client: &OAuthHttpClientAdapter,
    endpoint: &str,
    scopes: &[String],
    grants: &Option<Vec<String>>,
) -> Result<String> {
    let mut grant_types = vec![DEVICE_GRANT];
    if grants
        .as_ref()
        .is_none_or(|g| g.iter().any(|s| s == "refresh_token"))
    {
        grant_types.push("refresh_token");
    }
    let scope = scopes.join(" ");
    let request = Registration {
        client_name: "Codex",
        grant_types,
        token_endpoint_auth_method: "none",
        scope: (!scope.is_empty()).then_some(scope.as_str()),
    };
    let response = post(
        client,
        endpoint,
        "application/json",
        serde_json::to_vec(&request)?,
    )
    .await?;
    if !response.status().is_success() {
        return Err(provider_error(&response));
    }
    let registration: Registered = serde_json::from_slice(response.body())
        .context("invalid OAuth device client registration response")?;
    let id = registration.client_id.trim();
    if id.is_empty() {
        bail!("device client registration returned an empty client ID");
    }
    Ok(id.to_string())
}

struct DevicePkce {
    challenge: String,
    verifier: String,
}
impl DevicePkce {
    fn new() -> Self {
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        Self {
            challenge: challenge.as_str().to_string(),
            verifier: verifier.secret().to_string(),
        }
    }
}
#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: Option<u64>,
    interval: Option<u64>,
}
fn prompt(details: &DeviceCode, show: &mut impl FnMut(DeviceAuthorizationPrompt)) -> Result<()> {
    let uri = details
        .verification_uri_complete
        .as_ref()
        .unwrap_or(&details.verification_uri);
    let parsed = Url::parse(uri).context("invalid device verification URI")?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
    {
        bail!("invalid device verification URI");
    }
    if details.device_code.is_empty()
        || details.user_code.is_empty()
        || details.user_code.chars().any(char::is_control)
    {
        bail!("device authorization response has an invalid code");
    }
    show(DeviceAuthorizationPrompt {
        verification_uri: parsed.to_string(),
        user_code: details.user_code.clone(),
    });
    Ok(())
}
async fn request_device_code(
    client: &OAuthHttpClientAdapter,
    endpoint: &str,
    client_id: &str,
    scopes: &[String],
    resource: Option<&str>,
    pkce: Option<&DevicePkce>,
) -> Result<DeviceCode> {
    let mut fields = vec![("client_id", client_id.to_string())];
    if !scopes.is_empty() {
        fields.push(("scope", scopes.join(" ")));
    }
    if let Some(value) = resource.filter(|s| !s.trim().is_empty()) {
        fields.push(("resource", value.to_string()));
    }
    if let Some(pkce) = pkce {
        fields.push(("code_challenge", pkce.challenge.clone()));
        fields.push(("code_challenge_method", "S256".into()));
    }
    let response = post(
        client,
        endpoint,
        "application/x-www-form-urlencoded",
        form(&fields),
    )
    .await?;
    if !response.status().is_success() {
        return Err(provider_error(&response));
    }
    serde_json::from_slice(response.body()).context("invalid device authorization response")
}
async fn poll_token(
    client: &OAuthHttpClientAdapter,
    endpoint: &str,
    client_id: &str,
    resource: Option<&str>,
    details: &DeviceCode,
    pkce: Option<&DevicePkce>,
) -> Result<OAuthTokenResponse> {
    let deadline =
        Instant::now() + Duration::from_secs(details.expires_in.unwrap_or(900).clamp(1, 86_400));
    let mut interval = Duration::from_secs(details.interval.unwrap_or(5).max(1));
    loop {
        if Instant::now() >= deadline {
            bail!("OAuth device code expired");
        }
        let mut fields = vec![
            ("grant_type", DEVICE_GRANT.to_string()),
            ("device_code", details.device_code.clone()),
            ("client_id", client_id.to_string()),
        ];
        if let Some(value) = resource.filter(|s| !s.trim().is_empty()) {
            fields.push(("resource", value.to_string()));
        }
        if let Some(pkce) = pkce {
            fields.push(("code_verifier", pkce.verifier.clone()));
        }
        let response = tokio::time::timeout_at(
            deadline,
            post(
                client,
                endpoint,
                "application/x-www-form-urlencoded",
                form(&fields),
            ),
        )
        .await
        .map_err(|_| anyhow!("OAuth device code expired"))??;
        if Instant::now() >= deadline {
            bail!("OAuth device code expired");
        }
        if response.status().is_success() {
            return serde_json::from_slice(response.body())
                .context("invalid OAuth device token response");
        }
        let error = provider_error(&response);
        match error.downcast_ref::<ProviderError>().map(|e| e.0.as_str()) {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = interval.saturating_add(Duration::from_secs(5)),
            Some("expired_token") => bail!("OAuth device code expired"),
            Some("access_denied") => bail!("OAuth device authorization was denied"),
            _ => return Err(error),
        }
        tokio::time::sleep(interval.min(deadline.saturating_duration_since(Instant::now()))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn device_requests_do_not_follow_redirects_with_codes() {
        use codex_exec_server::RouteAwareHttpClient;
        use codex_http_client::HttpClientFactory;
        use codex_http_client::OutboundProxyPolicy;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::method;
        use wiremock::matchers::path;

        let provider = MockServer::start().await;
        let other = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/device"))
            .respond_with(
                ResponseTemplate::new(307)
                    .insert_header("location", format!("{}/capture", other.uri())),
            )
            .expect(1)
            .mount(&provider)
            .await;
        Mock::given(method("POST"))
            .and(path("/capture"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&other)
            .await;
        let client = OAuthHttpClientAdapter::new_with_redirect_mode(
            Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
                OutboundProxyPolicy::ReqwestDefault,
            ))),
            Default::default(),
            &format!("{}/mcp", provider.uri()),
            false,
            StreamableHttpRedirectMode::Legacy,
        )
        .unwrap();
        assert!(
            request_device_code(
                &client,
                &format!("{}/device", provider.uri()),
                "client",
                &[],
                None,
                None
            )
            .await
            .is_err()
        );
        provider.verify().await;
        other.verify().await;
    }

    #[tokio::test]
    async fn device_poll_handles_pending_and_sends_pkce_verifier() {
        use codex_exec_server::RouteAwareHttpClient;
        use codex_http_client::HttpClientFactory;
        use codex_http_client::OutboundProxyPolicy;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::body_string_contains;
        use wiremock::matchers::method;
        use wiremock::matchers::path;
        let provider = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let response_calls = calls.clone();
        Mock::given(method("POST")).and(path("/token"))
            .and(body_string_contains("code_verifier=verifier"))
            .respond_with(move |_: &wiremock::Request| {
                if response_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(400).set_body_json(serde_json::json!({"error":"authorization_pending"}))
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token":"synthetic-token", "token_type":"Bearer"}))
                }
            }).expect(2).mount(&provider).await;
        let client = OAuthHttpClientAdapter::new_with_redirect_mode(
            Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
                OutboundProxyPolicy::ReqwestDefault,
            ))),
            Default::default(),
            &format!("{}/mcp", provider.uri()),
            false,
            StreamableHttpRedirectMode::Legacy,
        )
        .unwrap();
        let details = DeviceCode {
            device_code: "synthetic-device".into(),
            user_code: "synthetic-user".into(),
            verification_uri: format!("{}/verify", provider.uri()),
            verification_uri_complete: None,
            expires_in: Some(10),
            interval: Some(1),
        };
        let pkce = DevicePkce {
            challenge: "challenge".into(),
            verifier: "verifier".into(),
        };
        assert!(
            poll_token(
                &client,
                &format!("{}/token", provider.uri()),
                "client",
                None,
                &details,
                Some(&pkce)
            )
            .await
            .is_ok()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        provider.verify().await;
    }

    #[test]
    fn provider_errors_do_not_echo_untrusted_response_bodies() {
        let response = oauth2::http::Response::builder()
            .status(400)
            .body(br#"{"error":"invalid request with a secret"}"#.to_vec())
            .unwrap();
        let message = provider_error(&response).to_string();
        assert!(!message.contains("secret"));
        assert!(message.contains("http_400"));
        let response = oauth2::http::Response::builder()
            .status(400)
            .body(br#"{"error":"topsecret"}"#.to_vec())
            .unwrap();
        assert!(!provider_error(&response).to_string().contains("topsecret"));
        let response = oauth2::http::Response::builder()
            .status(400)
            .body(br#"{"error":"authorization_pending"}"#.to_vec())
            .unwrap();
        assert!(
            provider_error(&response)
                .to_string()
                .contains("authorization_pending")
        );
    }

    #[test]
    fn prompt_rejects_non_web_verification_uris_and_empty_codes() {
        let mut code = DeviceCode {
            device_code: "device".into(),
            user_code: "user".into(),
            verification_uri: "javascript:alert(1)".into(),
            verification_uri_complete: None,
            expires_in: None,
            interval: None,
        };
        assert!(prompt(&code, &mut |_| panic!("invalid URI was shown")).is_err());
        code.verification_uri = "https://example.test/verify".into();
        code.user_code.clear();
        assert!(prompt(&code, &mut |_| panic!("empty code was shown")).is_err());
    }
}
