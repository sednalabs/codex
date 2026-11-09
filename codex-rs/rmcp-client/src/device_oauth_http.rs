//! Bounded same-origin redirects for device OAuth requests.
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use http::Method;
use http::header::CONTENT_LENGTH;
use http::header::CONTENT_TYPE;
use http::header::LOCATION;
use rmcp::transport::auth::OAuthHttpRedirectPolicy;
use tokio::time::Instant;

use crate::device_oauth_metadata::checked_endpoint;
use crate::oauth_http_client::OAuthHttpClientAdapter;

const MAX_REDIRECTS: usize = 10;

/// Retains the original endpoint origin and one deadline across all hops.
/// Credential-bearing bodies and resource-scoped headers never cross origins.
pub(crate) async fn execute_same_origin(
    client: &OAuthHttpClientAdapter,
    request: oauth2::HttpRequest,
    timeout: Duration,
) -> Result<oauth2::HttpResponse> {
    let (mut parts, mut body) = request.into_parts();
    let mut url = checked_endpoint(&parts.uri.to_string())?;
    let origin = url.origin();
    let deadline = Instant::now() + timeout;
    for redirects in 0..=MAX_REDIRECTS {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| anyhow!("OAuth device request timed out"))?;
        let mut request = oauth2::http::Request::builder()
            .method(parts.method.clone())
            .uri(url.as_str())
            .body(body.clone())?;
        *request.headers_mut() = parts.headers.clone();
        let response = tokio::time::timeout_at(
            deadline,
            client.execute_request(request, OAuthHttpRedirectPolicy::Stop, Some(remaining)),
        )
        .await
        .map_err(|_| anyhow!("OAuth device request timed out"))?
        .map_err(|_| anyhow!("OAuth device HTTP request failed"))?;
        let status = response.status().as_u16();
        if !matches!(status, 301 | 302 | 303 | 307 | 308) {
            return Ok(response);
        }
        let Some(location) = response.headers().get(LOCATION) else {
            return Ok(response);
        };
        let location = location
            .to_str()
            .context("invalid OAuth redirect location")?;
        let next = url.join(location).context("invalid OAuth redirect URL")?;
        checked_endpoint(next.as_str())?;
        if next.origin() != origin {
            return Ok(response);
        }
        if redirects == MAX_REDIRECTS {
            bail!("OAuth device request exceeded redirect limit");
        }
        if matches!(status, 301 | 302 | 303)
            && parts.method != Method::GET
            && parts.method != Method::HEAD
        {
            parts.method = Method::GET;
            parts.headers.remove(CONTENT_TYPE);
            parts.headers.remove(CONTENT_LENGTH);
            body.clear();
        }
        url = next;
    }
    unreachable!("the bounded redirect loop always returns")
}
