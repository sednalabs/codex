//! Credential-destination policy for enterprise MCP OAuth.

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use url::Host;
use url::Url;

/// A sanitized enterprise-auth failure that callers may handle without parsing text.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum EmaAuthFailure {
    #[error("invalid_grant")]
    InvalidGrant { grant_source: EmaInvalidGrantSource },
    #[error("insufficient_user_authentication")]
    InsufficientUserAuthentication,
    #[error("enterprise identity requires authentication")]
    ReauthenticationRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmaInvalidGrantSource {
    EnterpriseIdentity,
    ResourceAuthorization,
}

pub(crate) fn safe_oauth_error_code(code: Option<&str>) -> &str {
    code.filter(|code| {
        matches!(
            *code,
            "invalid_request"
                | "invalid_client"
                | "invalid_grant"
                | "invalid_scope"
                | "invalid_target"
                | "unauthorized_client"
                | "unsupported_grant_type"
                | "access_denied"
                | "temporarily_unavailable"
                | "server_error"
                | "insufficient_user_authentication"
        )
    })
    .unwrap_or("OAuth token request rejected")
}

pub(crate) fn validate_ema_oauth_endpoint(endpoint: &str, description: &str) -> Result<()> {
    let url = Url::parse(endpoint).with_context(|| format!("{description} is not a valid URL"))?;
    validate_credential_destination(&url, description)
}

fn validate_credential_destination(url: &Url, description: &str) -> Result<()> {
    let loopback = match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("{description} must use HTTPS or an HTTP loopback address");
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        bail!("{description} contains disallowed credentials or a URL fragment");
    }
    Ok(())
}
