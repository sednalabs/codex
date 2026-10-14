//! Bounded one-shot OAuth client-credentials exchange for the fixed Ops MCP target.
//!
//! The root-owned policy selects the endpoint and identity contract. This module
//! never discovers endpoints, follows redirects, retries, caches, or persists a
//! token. Ops MCP remains responsible for cryptographically verifying the JWT
//! against its configured JWKS before it authorizes a request.

use crate::provider_auth::SecretString;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::blocking::Request;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::Value;
use std::io::Read as _;
use std::time::Duration;
use url::Url;
use uuid::Uuid;
use zeroize::Zeroizing;

const MAX_TOKEN_RESPONSE_BYTES: usize = 16 * 1024;
const MAX_ACCESS_TOKEN_BYTES: usize = 12 * 1024;
const MAX_TOKEN_CLAIMS_BYTES: usize = 8 * 1024;
const TOKEN_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const REQUIRED_SCOPE: &str = "ops:write";

/// Root-selected client-credentials endpoint and exact protected MCP token contract.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpOAuthPolicy {
    pub(crate) token_endpoint: String,
    pub(crate) expected_issuer: String,
    pub(crate) client_id: String,
    pub(crate) audience: String,
    pub(crate) requested_scope: String,
}

pub(crate) struct McpAccessToken {
    pub(crate) bearer: SecretString,
    pub(crate) expires_at: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenEndpointResponse {
    access_token: SecretString,
    token_type: String,
    expires_in: Option<u64>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct AccessTokenHeader {
    alg: String,
    kid: String,
}

impl McpOAuthPolicy {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_https_url(&self.token_endpoint, "MCP OAuth token endpoint")?;
        validate_https_url(&self.expected_issuer, "MCP OAuth issuer")?;
        if self.client_id.is_empty()
            || self.client_id.len() > 512
            || !self
                .client_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._~-".contains(&byte))
            || self.audience.is_empty()
            || self.audience.len() > 1024
            || self.audience.chars().any(char::is_control)
            || self.requested_scope != REQUIRED_SCOPE
        {
            bail!("fixed MCP OAuth policy is outside the admitted token contract");
        }
        Ok(())
    }
}

pub(crate) fn exchange_client_credentials(
    policy: &McpOAuthPolicy,
    client_secret: &SecretString,
    principal: Uuid,
    project_id: u64,
) -> Result<McpAccessToken> {
    policy.validate()?;
    if principal.is_nil() || project_id == 0 || project_id > ((1_u64 << 53) - 1) {
        bail!("fixed MCP OAuth identity is outside the admitted token contract");
    }

    let client = Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(TOKEN_CONNECT_TIMEOUT)
        .timeout(TOKEN_REQUEST_TIMEOUT)
        .no_proxy()
        .build()
        .context("build bounded MCP OAuth client")?;
    let request = build_client_credentials_request(&client, policy, client_secret)?;
    let response = client
        .execute(request)
        .context("request fixed MCP client-credentials token")?;
    if response.status() != StatusCode::OK {
        bail!("fixed MCP OAuth endpoint returned a non-success response");
    }
    if !is_json_content_type(response.headers().get(CONTENT_TYPE)) {
        bail!("fixed MCP OAuth endpoint returned a non-JSON response");
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_TOKEN_RESPONSE_BYTES as u64)
    {
        bail!("fixed MCP OAuth response exceeds its size limit");
    }
    let mut body = Zeroizing::new(Vec::with_capacity(1024));
    response
        .take(MAX_TOKEN_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .context("read fixed MCP OAuth response")?;
    if body.len() > MAX_TOKEN_RESPONSE_BYTES {
        bail!("fixed MCP OAuth response exceeds its size limit");
    }

    let response: TokenEndpointResponse = serde_json::from_slice(&body)
        .context("decode fixed MCP OAuth token response")?;
    validate_token_response(response, policy, principal, project_id, unix_seconds()?)
}

fn build_client_credentials_request(
    client: &Client,
    policy: &McpOAuthPolicy,
    client_secret: &SecretString,
) -> Result<Request> {
    if client_secret.0.is_empty()
        || client_secret.0.len() > 4096
        || client_secret.0.chars().any(char::is_control)
    {
        bail!("fixed MCP OAuth client secret is invalid");
    }
    let encoded_client_id = form_encoded_component(&policy.client_id);
    let encoded_client_secret = Zeroizing::new(form_encoded_component(&client_secret.0));
    client
        .post(&policy.token_endpoint)
        .basic_auth(
            &encoded_client_id,
            Some(encoded_client_secret.as_str()),
        )
        .form(&[
            ("grant_type", "client_credentials"),
            ("audience", policy.audience.as_str()),
            ("scope", policy.requested_scope.as_str()),
        ])
        .build()
        .context("build fixed MCP client-credentials request")
}

fn form_encoded_component(value: &str) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("value", value);
    serializer
        .finish()
        .strip_prefix("value=")
        .unwrap_or_default()
        .to_string()
}

fn validate_token_response(
    response: TokenEndpointResponse,
    policy: &McpOAuthPolicy,
    principal: Uuid,
    project_id: u64,
    now: i64,
) -> Result<McpAccessToken> {
    if !response.token_type.eq_ignore_ascii_case("Bearer")
        || response
            .expires_in
            .is_some_and(|expires_in| expires_in == 0 || expires_in > 86_400)
        || response.scope.as_deref().is_some_and(|scope| {
            scope.split_whitespace().collect::<Vec<_>>() != [REQUIRED_SCOPE]
        })
        || response.access_token.0.is_empty()
        || response.access_token.0.len() > MAX_ACCESS_TOKEN_BYTES
        || response.access_token.0.chars().any(char::is_control)
    {
        bail!("fixed MCP OAuth token response is outside the admitted token contract");
    }

    let (header, claims) = parse_access_token_claims(&response.access_token.0)?;
    if !matches!(header.alg.as_str(), "RS256" | "RS512" | "ES256" | "ES384")
        || header.kid.is_empty()
        || header.kid.len() > 128
        || header.kid.chars().any(char::is_control)
    {
        bail!("fixed MCP OAuth access token has an unsupported signing header");
    }

    let issuer = claims.get("iss").and_then(Value::as_str);
    let subject = claims.get("sub").and_then(Value::as_str);
    let token_principal = subject.and_then(|value| Uuid::parse_str(value).ok());
    let expected_subject = principal.to_string();
    let client_identity_matches = ["azp", "client_id"].into_iter().all(|claim_name| {
        claims.get(claim_name).is_none_or(|value| {
            value
                .as_str()
                .is_some_and(|value| value.trim() == policy.client_id.as_str())
        })
    });
    let audiences_match = claim_audiences(&claims)
        .is_some_and(|audiences| audiences.as_slice() == [policy.audience.as_str()]);
    let scopes_match = claim_scopes(&claims)
        .is_some_and(|scopes| scopes.as_slice() == [REQUIRED_SCOPE]);
    let token_project = claim_project_id(&claims);
    let expires_at = claims.get("exp").and_then(Value::as_i64);
    let response_expires_at = response
        .expires_in
        .map(|expires_in| {
            let response_lifetime = i64::try_from(expires_in)
                .context("MCP OAuth token lifetime is outside the supported range")?;
            now.checked_add(response_lifetime)
                .context("MCP OAuth token expiry overflowed")
        })
        .transpose()?;

    if issuer != Some(policy.expected_issuer.as_str())
        || subject != Some(expected_subject.as_str())
        || token_principal != Some(principal)
        || !client_identity_matches
        || !audiences_match
        || !scopes_match
        || response.scope.as_deref().is_some_and(|scope| scope != REQUIRED_SCOPE)
        || token_project != Some(project_id)
        || expires_at.is_none_or(|expires| expires <= now)
    {
        bail!("fixed MCP OAuth access token does not match the selected identity policy");
    }

    let expires_at = expires_at.context("fixed MCP OAuth token has no expiry")?;
    // Respect both the JWT expiry and OAuth response lifetime. The downstream
    // Ops verifier still checks the JWT signature, issuer, audience, subject,
    // scope and expiry against its current JWKS/configuration.
    Ok(McpAccessToken {
        bearer: response.access_token,
        expires_at: response_expires_at.map_or(expires_at, |limit| expires_at.min(limit)),
    })
}

fn parse_access_token_claims(token: &str) -> Result<(AccessTokenHeader, Value)> {
    let mut parts = token.split('.');
    let header_segment = parts.next().unwrap_or_default();
    let claims_segment = parts.next().unwrap_or_default();
    let signature_segment = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || header_segment.is_empty()
        || claims_segment.is_empty()
        || signature_segment.is_empty()
    {
        bail!("fixed MCP access token is not a compact signed JWT");
    }
    let header_bytes = URL_SAFE_NO_PAD
        .decode(header_segment)
        .context("decode fixed MCP access-token header")?;
    let claims_bytes = URL_SAFE_NO_PAD
        .decode(claims_segment)
        .context("decode fixed MCP access-token claims")?;
    if header_bytes.len() > 1024 || claims_bytes.len() > MAX_TOKEN_CLAIMS_BYTES {
        bail!("fixed MCP access-token claims exceed their size limit");
    }
    let signature = URL_SAFE_NO_PAD
        .decode(signature_segment)
        .context("decode fixed MCP access-token signature segment")?;
    if signature.is_empty() || signature.len() > 1024 {
        bail!("fixed MCP access-token signature segment is invalid");
    }
    let header: AccessTokenHeader =
        serde_json::from_slice(&header_bytes).context("decode fixed MCP access-token header")?;
    let claims: Value =
        serde_json::from_slice(&claims_bytes).context("decode fixed MCP access-token claims")?;
    Ok((header, claims))
}

fn claim_audiences(claims: &Value) -> Option<Vec<&str>> {
    match claims.get("aud") {
        Some(Value::String(value)) => Some(value.split_whitespace().collect()),
        Some(Value::Array(values)) => values
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>(),
        _ => None,
    }
}

fn claim_scopes(claims: &Value) -> Option<Vec<&str>> {
    let value = claims
        .get("scope")
        .or_else(|| claims.get("scp"))
        .or_else(|| claims.get("scopes"));
    match value {
        Some(Value::String(scopes)) => Some(scopes.split_whitespace().collect()),
        Some(Value::Array(scopes)) => scopes
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>(),
        _ => None,
    }
}

fn claim_project_id(claims: &Value) -> Option<u64> {
    match claims.get("project_id")? {
        Value::Number(value) => value.as_u64(),
        Value::String(value) => value.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn validate_https_url(value: &str, name: &str) -> Result<()> {
    let url = Url::parse(value).with_context(|| format!("parse fixed {name}"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.as_str() != value
    {
        bail!("fixed {name} must be canonical HTTPS without userinfo, query, or fragment");
    }
    Ok(())
}

fn is_json_content_type(value: Option<&reqwest::header::HeaderValue>) -> bool {
    value
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| {
            let media_type = media_type.trim().to_ascii_lowercase();
            media_type == "application/json"
                || media_type
                    .rsplit_once('/')
                    .is_some_and(|(_, subtype)| subtype.ends_with("+json"))
        })
}

fn unix_seconds() -> Result<i64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_secs() as i64)
}

#[cfg(test)]
#[path = "mcp_oauth_tests.rs"]
mod tests;
