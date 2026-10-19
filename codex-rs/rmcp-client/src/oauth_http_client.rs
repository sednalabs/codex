use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_exec_server::HttpClient;
use codex_exec_server::HttpHeader;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use codex_http_client::HttpClient as DirectHttpClient;
use futures::StreamExt;
use oauth2::HttpRequest;
use oauth2::HttpResponse;
use reqwest::header::HeaderMap;
use rmcp::transport::auth::OAuthHttpClient;
use rmcp::transport::auth::OAuthHttpClientError;
use rmcp::transport::auth::OAuthHttpClientFuture;
use rmcp::transport::auth::OAuthHttpRedirectPolicy;
use rmcp::transport::auth::OAuthHttpRequest;

use crate::auth_status::OAuthDiscoveryTimeout;

const MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES: usize = 1024 * 1024;
static NEXT_OAUTH_REQUEST_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub(crate) struct OAuthHttpClientAdapter {
    http_client: OAuthHttpClientBackend,
    default_headers: HeaderMap,
    timeout: OAuthDiscoveryTimeout,
}

#[derive(Clone)]
enum OAuthHttpClientBackend {
    ExecServer(Arc<dyn HttpClient>),
    Direct(DirectHttpClient),
}

impl OAuthHttpClientAdapter {
    pub(crate) fn new(http_client: Arc<dyn HttpClient>, default_headers: HeaderMap) -> Self {
        Self {
            http_client: OAuthHttpClientBackend::ExecServer(http_client),
            default_headers,
            timeout: OAuthDiscoveryTimeout::Requested,
        }
    }

    pub(crate) fn new_with_max_timeout(
        http_client: Arc<dyn HttpClient>,
        default_headers: HeaderMap,
        max_timeout: Duration,
    ) -> Self {
        Self {
            http_client: OAuthHttpClientBackend::ExecServer(http_client),
            default_headers,
            timeout: OAuthDiscoveryTimeout::Capped(max_timeout),
        }
    }

    pub(crate) fn new_direct_with_max_timeout(
        http_client: DirectHttpClient,
        default_headers: HeaderMap,
        max_timeout: Duration,
    ) -> Self {
        Self {
            http_client: OAuthHttpClientBackend::Direct(http_client),
            default_headers,
            timeout: OAuthDiscoveryTimeout::Capped(max_timeout),
        }
    }

    async fn execute_request(
        &self,
        request: HttpRequest,
        redirect_policy: OAuthHttpRedirectPolicy,
        timeout: Option<Duration>,
    ) -> Result<HttpResponse, OAuthHttpClientError> {
        let redirect_policy = match redirect_policy {
            OAuthHttpRedirectPolicy::Follow => HttpRedirectPolicy::Follow,
            OAuthHttpRedirectPolicy::Stop => HttpRedirectPolicy::Stop,
            _ => {
                return Err(OAuthHttpClientError::from(
                    "unsupported OAuth HTTP redirect policy",
                ));
            }
        };
        let (parts, body) = request.into_parts();
        let mut headers = self.default_headers.clone();
        for name in parts.headers.keys() {
            headers.remove(name);
        }
        headers.extend(parts.headers);
        let timeout = match self.timeout {
            OAuthDiscoveryTimeout::Requested => timeout,
            OAuthDiscoveryTimeout::Capped(max_timeout) => {
                Some(timeout.map_or(max_timeout, |timeout| timeout.min(max_timeout)))
            }
        };
        let timeout_ms = timeout.map(|timeout| {
            u64::try_from(timeout.as_millis())
                .unwrap_or(u64::MAX)
                .max(1)
        });
        let request_id = NEXT_OAUTH_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let (status, response_headers, response_body) = match &self.http_client {
            OAuthHttpClientBackend::ExecServer(http_client) => {
                let headers = headers
                    .iter()
                    .map(|(name, value)| {
                        Ok(HttpHeader {
                            name: name.as_str().to_string(),
                            value: value
                                .to_str()
                                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?
                                .to_string(),
                        })
                    })
                    .collect::<Result<Vec<_>, OAuthHttpClientError>>()?;
                let (response, mut body_stream) = http_client
                    .http_request_stream(HttpRequestParams {
                        method: parts.method.to_string(),
                        url: parts.uri.to_string(),
                        headers,
                        body: (!body.is_empty()).then_some(body.into()),
                        timeout_ms,
                        redirect_policy,
                        request_id: format!("oauth-request-{request_id}"),
                        stream_response: true,
                    })
                    .await
                    .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
                let mut response_body = Vec::new();
                while let Some(chunk) = body_stream
                    .recv()
                    .await
                    .map_err(|error| OAuthHttpClientError::from(error.to_string()))?
                {
                    append_bounded_body(&mut response_body, &chunk)?;
                }
                (response.status, response.headers, response_body)
            }
            OAuthHttpClientBackend::Direct(http_client) => {
                let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
                    .map_err(|_| OAuthHttpClientError::from("invalid OAuth HTTP method"))?;
                let mut request = http_client
                    .request(method, parts.uri.to_string())
                    .headers(headers);
                if !body.is_empty() {
                    request = request.body(body);
                }
                if let Some(timeout) = timeout {
                    request = request.timeout(timeout);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
                let status = response.status().as_u16();
                let response_headers = response
                    .headers()
                    .iter()
                    .map(|(name, value)| {
                        Ok(HttpHeader {
                            name: name.as_str().to_string(),
                            value: value
                                .to_str()
                                .map_err(|error| OAuthHttpClientError::from(error.to_string()))?
                                .to_string(),
                        })
                    })
                    .collect::<Result<Vec<_>, OAuthHttpClientError>>()?;
                let mut body_stream = response.bytes_stream();
                let mut response_body = Vec::new();
                while let Some(chunk) = body_stream.next().await {
                    let chunk =
                        chunk.map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
                    append_bounded_body(&mut response_body, &chunk)?;
                }
                (status, response_headers, response_body)
            }
        };
        let mut builder = oauth2::http::Response::builder().status(status);
        for header in response_headers {
            builder = builder.header(header.name, header.value);
        }
        builder
            .body(response_body)
            .map_err(|error| OAuthHttpClientError::from(error.to_string()))
    }
}

fn append_bounded_body(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), OAuthHttpClientError> {
    if chunk.len() > MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES - body.len() {
        return Err(OAuthHttpClientError::from(format!(
            "OAuth HTTP response body exceeds {MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES} bytes"
        )));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

impl OAuthHttpClient for OAuthHttpClientAdapter {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(self.execute_request(request.request, request.redirect_policy, request.timeout))
    }
}
