//! Device-grant discovery. Never infer credential-bearing endpoints.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use rmcp::transport::AuthorizationManager;
use rmcp::transport::auth::AuthError;
use rmcp::transport::auth::AuthorizationMetadata;
use rmcp::transport::auth::OAuthHttpClient;
use serde::Deserialize;
use url::Url;

use crate::device_oauth_http::execute_same_origin;
use crate::oauth::validate_authorization_server_endpoints;
use crate::oauth_http_client::OAuthHttpClientAdapter;

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DeviceMetadata {
    pub issuer: String,
    pub token_endpoint: String,
    pub device_authorization_endpoint: String,
    pub registration_endpoint: Option<String>,
    pub grant_types_supported: Option<Vec<String>>,
    pub scopes_supported: Option<Vec<String>>,
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(flatten)]
    additional_fields: HashMap<String, serde_json::Value>,
}

pub(crate) fn checked_endpoint(value: &str) -> Result<Url> {
    let url = Url::parse(value).context("invalid OAuth device endpoint URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("OAuth device endpoint must be an HTTP(S) URL without credentials or fragment");
    }
    Ok(url)
}

impl DeviceMetadata {
    /// RMCP requires a syntactic authorization URL even for a refresh-only client.
    /// No browser flow is advertised or permitted through this synthetic metadata.
    pub(crate) fn into_refresh_metadata(self) -> AuthorizationMetadata {
        let mut metadata = AuthorizationMetadata::default();
        metadata.authorization_endpoint = self.issuer.clone();
        metadata.token_endpoint = self.token_endpoint;
        metadata.issuer = Some(self.issuer);
        metadata.scopes_supported = self.scopes_supported;
        metadata.response_types_supported = Some(Vec::new());
        metadata.additional_fields = self.additional_fields;
        metadata
    }

    fn validate(&self) -> Result<()> {
        let issuer = checked_endpoint(&self.issuer)?;
        if issuer.query().is_some() {
            bail!("OAuth issuer must not have a query");
        }
        checked_endpoint(&self.token_endpoint)?;
        checked_endpoint(&self.device_authorization_endpoint)?;
        if let Some(endpoint) = &self.registration_endpoint {
            checked_endpoint(endpoint)?;
        }
        if self
            .grant_types_supported
            .as_ref()
            .is_some_and(|grants| !grants.iter().any(|grant| grant == DEVICE_GRANT))
        {
            bail!("OAuth device grant is not advertised");
        }
        Ok(())
    }
}

// Fallback is deliberately limited to issuer identity derived from the configured
// resource URL. Standard protected-resource discovery remains owned by rmcp.
fn device_only_issuer_matches(resource: &str, metadata_path: &str, issuer: &str) -> bool {
    let Ok(parsed) = checked_endpoint(issuer) else {
        return false;
    };
    if parsed.query().is_some() {
        return false;
    }
    let resource = resource.split(['?', '#']).next().unwrap_or(resource);
    if metadata_path == "/.well-known/oauth-authorization-server" {
        let Some((scheme, remainder)) = resource.split_once("://") else {
            return false;
        };
        let end = scheme.len() + 3 + remainder.find('/').unwrap_or(remainder.len());
        let origin = &resource[..end];
        issuer == origin || issuer.strip_suffix('/') == Some(origin)
    } else {
        issuer == resource
    }
}

fn discovery_paths(base_path: &str) -> Vec<String> {
    let path = base_path.trim_matches('/');
    let canonical = "/.well-known/oauth-authorization-server";
    if path.is_empty() {
        vec![canonical.to_string()]
    } else {
        vec![
            format!("{canonical}/{path}"),
            format!("/{path}{canonical}"),
            canonical.to_string(),
        ]
    }
}

pub(crate) async fn discover_device_metadata(
    resource: &str,
    client: Arc<OAuthHttpClientAdapter>,
) -> Result<DeviceMetadata> {
    checked_endpoint(resource)?;
    let manager_client: Arc<dyn OAuthHttpClient> = client.clone();
    let manager =
        AuthorizationManager::new_with_oauth_http_client(resource, manager_client).await?;
    match manager.resolve_metadata().await {
        Ok(resolution) if resolution.source.is_discovered() => {
            let metadata = resolution.metadata;
            validate_authorization_server_endpoints(&metadata)?;
            let discovery: DeviceMetadata = serde_json::from_value(serde_json::to_value(metadata)?)
                .context("authorization metadata does not advertise device login")?;
            discovery.validate()?;
            Ok(discovery)
        }
        Ok(_) | Err(AuthError::NoAuthorizationSupport) => {
            discover_device_only_metadata(resource, &client)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("no issuer-bound OAuth device metadata was discovered")
                })
        }
        Err(error) => Err(error.into()),
    }
}

/// Narrow fallback for servers whose metadata omits the browser authorization endpoint.
/// A missing or mismatched issuer is never treated as discovered support.
pub(crate) async fn discover_device_only_metadata(
    resource: &str,
    client: &OAuthHttpClientAdapter,
) -> Result<Option<DeviceMetadata>> {
    let resource_url = checked_endpoint(resource)?;
    for path in discovery_paths(resource_url.path()) {
        let mut url = resource_url.clone();
        url.set_path(&path);
        url.set_query(None);
        let request = oauth2::http::Request::builder()
            .uri(url.as_str())
            .header("MCP-Protocol-Version", "2024-11-05")
            .body(Vec::new())?;
        let response = match execute_same_origin(client, request, Duration::from_secs(5)).await {
            Ok(response) if response.status().is_success() => response,
            _ => continue,
        };
        let metadata: DeviceMetadata = match serde_json::from_slice(response.body()) {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if metadata.authorization_endpoint.is_none()
            && device_only_issuer_matches(resource, &path, &metadata.issuer)
        {
            metadata.validate()?;
            return Ok(Some(metadata));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_only_issuer_uses_exact_discovery_identity() {
        assert!(device_only_issuer_matches(
            "https://example.test/mcp",
            "/.well-known/oauth-authorization-server/mcp",
            "https://example.test/mcp"
        ));
        assert!(!device_only_issuer_matches(
            "https://example.test/mcp",
            "/.well-known/oauth-authorization-server/mcp",
            "https://example.test/other"
        ));
        assert!(device_only_issuer_matches(
            "https://example.test/mcp",
            "/.well-known/oauth-authorization-server",
            "https://example.test/"
        ));
        assert!(!device_only_issuer_matches(
            "https://example.test/mcp",
            "/.well-known/oauth-authorization-server",
            "https://evil.test/"
        ));
        assert!(!device_only_issuer_matches(
            "https://example.test/mcp",
            "/.well-known/oauth-authorization-server",
            "https://user@example.test/"
        ));
    }

    #[tokio::test]
    async fn device_only_metadata_requires_the_exact_discovery_issuer() {
        use codex_exec_server::RouteAwareHttpClient;
        use codex_http_client::HttpClientFactory;
        use codex_http_client::OutboundProxyPolicy;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::method;
        use wiremock::matchers::path;
        let server = MockServer::start().await;
        let resource = format!("{}/mcp", server.uri());
        let metadata = serde_json::json!({
            "issuer": resource,
            "device_authorization_endpoint": format!("{}/device", server.uri()),
            "token_endpoint": format!("{}/token", server.uri()),
            "grant_types_supported": [DEVICE_GRANT],
        });
        Mock::given(method("GET"))
            .and(path("/.well-known/oauth-authorization-server/mcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(metadata))
            .mount(&server)
            .await;
        let adapter = Arc::new(
            OAuthHttpClientAdapter::new_with_redirect_mode(
                Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
                    OutboundProxyPolicy::ReqwestDefault,
                ))),
                Default::default(),
                &resource,
                false,
                crate::http_client_adapter::StreamableHttpRedirectMode::Legacy,
            )
            .unwrap(),
        );
        let metadata = discover_device_metadata(&resource, adapter)
            .await
            .expect("device-only metadata");
        assert_eq!(metadata.issuer, resource);
        assert_eq!(
            metadata.device_authorization_endpoint,
            format!("{}/device", server.uri())
        );
    }

    #[test]
    fn invalid_endpoints_and_unadvertised_grants_fail_closed() {
        let mut metadata = DeviceMetadata {
            issuer: "https://example.test".into(),
            token_endpoint: "https://example.test/token".into(),
            device_authorization_endpoint: "https://example.test/device".into(),
            registration_endpoint: None,
            grant_types_supported: None,
            scopes_supported: None,
            authorization_endpoint: None,
            additional_fields: HashMap::new(),
        };
        assert!(metadata.validate().is_ok());
        metadata.device_authorization_endpoint = "https://user:password@example.test/device".into();
        assert!(metadata.validate().is_err());
        metadata.device_authorization_endpoint = "https://example.test/device".into();
        metadata.grant_types_supported = Some(vec!["authorization_code".into()]);
        assert!(metadata.validate().is_err());
    }
}
