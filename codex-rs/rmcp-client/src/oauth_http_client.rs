use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use codex_exec_server::HttpClient;
use codex_exec_server::HttpHeader;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use codex_http_client::HttpClient as DirectHttpClient;
use futures::StreamExt;
use futures::future::BoxFuture;
use oauth2::HttpRequest;
use oauth2::HttpResponse;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use reqwest::header::USER_AGENT;
use url::Origin;
use url::Url;
use rmcp::transport::auth::OAuthHttpClient;
use rmcp::transport::auth::OAuthHttpClientError;
use rmcp::transport::auth::OAuthHttpClientFuture;
use rmcp::transport::auth::OAuthHttpRedirectPolicy;
use rmcp::transport::auth::OAuthHttpRequest;

use crate::auth_status::OAuthDiscoveryTimeout;
use crate::utils::MCP_USER_AGENT;

const MAX_OAUTH_HTTP_RESPONSE_BODY_BYTES: usize = 1024 * 1024;
const MAX_OAUTH_HTTP_REDIRECTS: usize = 10;
static NEXT_OAUTH_REQUEST_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub(crate) struct OAuthHttpClientAdapter {
    http_client: OAuthHttpClientBackend,
    default_headers: HeaderMap,
    resource_origin: Option<Origin>,
    timeout: OAuthDiscoveryTimeout,
    fixed_api_exchange_observer: Option<FixedApiExchangeObserver>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FixedApiExchangeFailure {
    Unknown,
    Transport,
    HttpResponse {
        status: u16,
        oauth_error: Option<&'static str>,
    },
}

impl FixedApiExchangeFailure {
    pub(crate) fn safe_detail(self) -> String {
        match self {
            Self::Unknown => "unknown".to_string(),
            Self::Transport => "transport".to_string(),
            Self::HttpResponse {
                status,
                oauth_error: Some(code),
            } => format!("http_status_{status}_oauth_{code}"),
            Self::HttpResponse {
                status,
                oauth_error: None,
            } => format!("http_status_{status}"),
        }
    }
}

/// Captures only a closed classification from the fixed Ops API token request.
#[derive(Clone, Default)]
pub(crate) struct FixedApiExchangeObserver {
    failure: Arc<Mutex<Option<FixedApiExchangeFailure>>>,
}

impl FixedApiExchangeObserver {
    pub(crate) fn record_transport_failure(&self) {
        self.record(FixedApiExchangeFailure::Transport);
    }

    pub(crate) fn record_response(&self, status: u16, body: &[u8]) {
        self.record(FixedApiExchangeFailure::HttpResponse {
            status,
            oauth_error: if (200..300).contains(&status) {
                None
            } else {
                oauth_error_code(body)
            },
        });
    }

    pub(crate) fn failure(&self) -> Option<FixedApiExchangeFailure> {
        self.failure.lock().ok().and_then(|failure| *failure)
    }

    fn record_result(&self, response: &Result<HttpResponse, OAuthHttpClientError>) {
        match response {
            Ok(response) => self.record_response(response.status().as_u16(), response.body()),
            Err(_) => self.record_transport_failure(),
        }
    }

    fn record(&self, failure: FixedApiExchangeFailure) {
        if let Ok(mut recorded) = self.failure.lock()
            && recorded.is_none()
        {
            *recorded = Some(failure);
        }
    }
}

fn oauth_error_code(body: &[u8]) -> Option<&'static str> {
    #[derive(serde::Deserialize)]
    struct ErrorCodeResponse {
        error: Option<String>,
    }

    // Unknown response fields, including token fields, are ignored by serde's
    // struct visitor instead of being retained in a general JSON value.
    let response = serde_json::from_slice::<ErrorCodeResponse>(body).ok()?;
    let code = response.error?;
    match code.as_str() {
        "invalid_client" => Some("invalid_client"),
        "invalid_grant" => Some("invalid_grant"),
        "unauthorized_client" => Some("unauthorized_client"),
        "access_denied" => Some("access_denied"),
        "invalid_scope" => Some("invalid_scope"),
        "invalid_request" => Some("invalid_request"),
        _ => None,
    }
}

#[derive(Clone)]
enum OAuthHttpClientBackend {
    ExecServer(Arc<dyn HttpClient>),
    Direct(DirectHttpClient),
}

impl OAuthHttpClientAdapter {
    #[cfg(test)]
    pub(crate) fn new(http_client: Arc<dyn HttpClient>, default_headers: HeaderMap) -> Self {
        Self {
            http_client: OAuthHttpClientBackend::ExecServer(http_client),
            default_headers,
            resource_origin: None,
            timeout: OAuthDiscoveryTimeout::Requested,
            fixed_api_exchange_observer: None,
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
            resource_origin: None,
            timeout: OAuthDiscoveryTimeout::Capped(max_timeout),
            fixed_api_exchange_observer: None,
        }
    }

    pub(crate) fn new_scoped(
        http_client: Arc<dyn HttpClient>,
        default_headers: HeaderMap,
        resource_url: &str,
        timeout: OAuthDiscoveryTimeout,
    ) -> Result<Self, url::ParseError> {
        Ok(Self {
            http_client: OAuthHttpClientBackend::ExecServer(http_client),
            default_headers,
            resource_origin: Some(Url::parse(resource_url)?.origin()),
            timeout,
            fixed_api_exchange_observer: None,
        })
    }

    pub(crate) fn new_direct_scoped(
        http_client: DirectHttpClient,
        default_headers: HeaderMap,
        resource_url: &str,
        timeout: OAuthDiscoveryTimeout,
    ) -> Result<Self, url::ParseError> {
        Ok(Self {
            http_client: OAuthHttpClientBackend::Direct(http_client),
            default_headers,
            resource_origin: Some(Url::parse(resource_url)?.origin()),
            timeout,
            fixed_api_exchange_observer: None,
        })
    }

    /// Attaches the bounded observer used by the fixed Ops API login/refresh paths.
    pub(crate) fn with_fixed_api_exchange_observer(
        mut self,
        observer: FixedApiExchangeObserver,
    ) -> Self {
        self.fixed_api_exchange_observer = Some(observer);
        self
    }

    pub(crate) async fn execute_request(
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
        let mut request_url = self.resource_origin.as_ref()
            .map(|_| Url::parse(&parts.uri.to_string()))
            .transpose()
            .map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
        let is_resource_origin = match (&self.resource_origin, &request_url) {
            (Some(origin), Some(url)) => url.origin() == *origin,
            _ => true,
        };
        let mut headers = if is_resource_origin {
            self.default_headers.clone()
        } else {
            HeaderMap::new()
        };
        for name in parts.headers.keys() {
            headers.remove(name);
        }
        let has_resource_only_headers = self.resource_origin.is_some()
            && is_resource_origin
            && headers.iter().any(|(name, value)| {
                name != USER_AGENT || value != HeaderValue::from_static(MCP_USER_AGENT)
            });
        headers.extend(parts.headers);
        if !is_resource_origin {
            headers.insert(USER_AGENT, HeaderValue::from_static(MCP_USER_AGENT));
        }
        let follow_same_origin_redirects =
            has_resource_only_headers && redirect_policy == HttpRedirectPolicy::Follow;
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
                let deadline = timeout.map(|timeout| Instant::now() + timeout);
                let mut params = HttpRequestParams {
                    method: parts.method.to_string(),
                    url: parts.uri.to_string(),
                    headers,
                    body: (!body.is_empty()).then_some(body.into()),
                    timeout_ms,
                    redirect_policy: if follow_same_origin_redirects {
                        HttpRedirectPolicy::Stop
                    } else {
                        redirect_policy
                    },
                    request_id: String::new(),
                    stream_response: true,
                };
                let mut redirects = 0;
                loop {
                    let request_id = NEXT_OAUTH_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
                    params.request_id = format!("oauth-request-{request_id}");
                    let (response, mut body_stream) = http_client
                        .http_request_stream(params.clone())
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
                    if !follow_same_origin_redirects
                        || !matches!(response.status, 301 | 302 | 303 | 307 | 308)
                    {
                        break (response.status, response.headers, response_body);
                    }
                    let next_url = response.headers.iter()
                        .find(|header| header.name.eq_ignore_ascii_case("location"))
                        .and_then(|header| request_url.as_ref()?.join(&header.value).ok())
                        .filter(|url| self.resource_origin.as_ref().is_some_and(|origin| url.origin() == *origin));
                    let Some(next_url) = next_url else {
                        break (response.status, response.headers, response_body);
                    };
                    if redirects >= MAX_OAUTH_HTTP_REDIRECTS {
                        return Err(OAuthHttpClientError::from("OAuth HTTP request exceeded redirect limit"));
                    }
                    if response.status == 303
                        || (matches!(response.status, 301 | 302) && params.method == "POST")
                    {
                        if params.method != "HEAD" {
                            params.method = "GET".to_string();
                        }
                        params.body = None;
                        params.headers.retain(|header| {
                            !["content-type", "content-length", "content-encoding", "transfer-encoding"]
                                .iter()
                                .any(|name| header.name.eq_ignore_ascii_case(name))
                        });
                    }
                    params.url = next_url.to_string();
                    if let Some(deadline) = deadline {
                        let remaining = deadline.checked_duration_since(Instant::now())
                            .ok_or_else(|| OAuthHttpClientError::from("OAuth HTTP request timed out"))?;
                        params.timeout_ms = Some(
                            u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX).max(1),
                        );
                    }
                    request_url = Some(next_url);
                    redirects += 1;
                }
            }
            OAuthHttpClientBackend::Direct(http_client) => {
                let mut method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())
                    .map_err(|_| OAuthHttpClientError::from("invalid OAuth HTTP method"))?;
                let mut request_body = body;
                let mut current_url = parts.uri.to_string();
                let redirect_origin = request_url.as_ref().map(Url::origin);
                let deadline = timeout.map(|timeout| Instant::now() + timeout);
                let follow_scoped = self.resource_origin.is_some()
                    && redirect_policy == HttpRedirectPolicy::Follow;
                let mut redirects = 0;
                loop {
                    let mut request = http_client
                        .request(method.clone(), current_url.clone())
                        .headers(headers.clone());
                    if !request_body.is_empty() {
                        request = request.body(request_body.clone());
                    }
                    if let Some(deadline) = deadline {
                        let remaining = deadline.checked_duration_since(Instant::now())
                            .ok_or_else(|| OAuthHttpClientError::from("OAuth HTTP request timed out"))?;
                        request = request.timeout(remaining);
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
                        let chunk = chunk.map_err(|error| OAuthHttpClientError::from(error.to_string()))?;
                        append_bounded_body(&mut response_body, &chunk)?;
                    }
                    if !follow_scoped || !matches!(status, 301 | 302 | 303 | 307 | 308) {
                        break (status, response_headers, response_body);
                    }
                    let next_url = response_headers.iter()
                        .find(|header| header.name.eq_ignore_ascii_case("location"))
                        .and_then(|header| request_url.as_ref()?.join(&header.value).ok())
                        .filter(|url| redirect_origin.as_ref().is_some_and(|origin| url.origin() == *origin));
                    let Some(next_url) = next_url else {
                        break (status, response_headers, response_body);
                    };
                    if redirects >= MAX_OAUTH_HTTP_REDIRECTS {
                        return Err(OAuthHttpClientError::from("OAuth HTTP request exceeded redirect limit"));
                    }
                    if status == 303 || (matches!(status, 301 | 302) && method == reqwest::Method::POST) {
                        if method != reqwest::Method::HEAD {
                            method = reqwest::Method::GET;
                        }
                        request_body.clear();
                        for name in ["content-type", "content-length", "content-encoding", "transfer-encoding"] {
                            headers.remove(name);
                        }
                    }
                    current_url = next_url.to_string();
                    request_url = Some(next_url);
                    redirects += 1;
                }
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
        let is_post = request.request.method().as_str() == "POST";
        let observer = self.fixed_api_exchange_observer.clone();
        let response =
            self.execute_request(request.request, request.redirect_policy, request.timeout);
        Box::pin(async move {
            let response = response.await;
            if is_post && let Some(observer) = observer {
                observer.record_result(&response);
            }
            response
        })
    }
}

impl<'client> oauth2::AsyncHttpClient<'client> for OAuthHttpClientAdapter {
    type Error = std::io::Error;
    type Future = BoxFuture<'client, Result<HttpResponse, Self::Error>>;

    fn call(&'client self, request: HttpRequest) -> Self::Future {
        let is_post = request.method().as_str() == "POST";
        let observer = self.fixed_api_exchange_observer.clone();
        Box::pin(async move {
            let response = self
                .execute_request(
                    request,
                    OAuthHttpRedirectPolicy::Stop,
                    Some(Duration::from_secs(10)),
                )
                .await;
            if is_post && let Some(observer) = observer {
                observer.record_result(&response);
            }
            response.map_err(std::io::Error::other)
        })
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use codex_exec_server::RouteAwareHttpClient;
    use codex_http_client::HttpClientFactory;
    use codex_http_client::OutboundProxyPolicy;
    use std::collections::HashMap;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[tokio::test]
    async fn resource_headers_are_not_forwarded_to_cross_origin_oauth_endpoints() -> anyhow::Result<()> {
        let resource = MockServer::start().await;
        let destination = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&destination)
            .await;
        let defaults = crate::utils::build_default_headers(
            Some(HashMap::from([("x-api-key".to_string(), "synthetic-secret".to_string())])),
            /*env_http_headers*/ None,
        )?;
        let adapter = OAuthHttpClientAdapter::new_scoped(
            Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
                OutboundProxyPolicy::ReqwestDefault,
            ))),
            defaults,
            &format!("{}/mcp", resource.uri()),
            OAuthDiscoveryTimeout::Requested,
        )?;
        adapter.execute_request(
            oauth2::http::Request::builder()
                .method("POST")
                .uri(format!("{}/register", destination.uri()))
                .body(Vec::new())?,
            OAuthHttpRedirectPolicy::Stop,
            Some(Duration::from_secs(10)),
        ).await.map_err(|error| anyhow::anyhow!(error))?;
        let requests = destination.received_requests().await.expect("recorded requests");
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].headers.contains_key("x-api-key"));
        Ok(())
    }

    #[tokio::test]
    async fn same_origin_redirect_preserves_configured_resource_headers() -> anyhow::Result<()> {
        let resource = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .and(header("x-api-key", "synthetic-secret"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", "/registered"))
            .expect(1)
            .mount(&resource)
            .await;
        Mock::given(method("POST"))
            .and(path("/registered"))
            .and(header("x-api-key", "synthetic-secret"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&resource)
            .await;
        let defaults = crate::utils::build_default_headers(
            Some(HashMap::from([("x-api-key".to_string(), "synthetic-secret".to_string())])),
            /*env_http_headers*/ None,
        )?;
        let adapter = OAuthHttpClientAdapter::new_scoped(
            Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
                OutboundProxyPolicy::ReqwestDefault,
            ))),
            defaults,
            &format!("{}/mcp", resource.uri()),
            OAuthDiscoveryTimeout::Requested,
        )?;
        let response = adapter.execute_request(
            oauth2::http::Request::builder()
                .method("POST")
                .uri(format!("{}/register", resource.uri()))
                .body(Vec::new())?,
            OAuthHttpRedirectPolicy::Follow,
            Some(Duration::from_secs(10)),
        ).await.map_err(|error| anyhow::anyhow!(error))?;
        assert_eq!(response.status(), oauth2::http::StatusCode::CREATED);
        resource.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn direct_scoped_discovery_preserves_same_origin_headers() -> anyhow::Result<()> {
        let resource = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/metadata"))
            .and(header("x-api-key", "synthetic-secret"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/metadata-final"))
            .expect(1)
            .mount(&resource)
            .await;
        Mock::given(method("GET"))
            .and(path("/metadata-final"))
            .and(header("x-api-key", "synthetic-secret"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&resource)
            .await;
        let defaults = crate::utils::build_default_headers(
            Some(HashMap::from([("x-api-key".to_string(), "synthetic-secret".to_string())])),
            /*env_http_headers*/ None,
        )?;
        let client = codex_http_client::HttpClientBuilder::new()
            .without_redirects()
            .without_retries()
            .without_request_logging()
            .build_direct()?;
        let adapter = OAuthHttpClientAdapter::new_direct_scoped(
            client,
            defaults,
            &format!("{}/mcp", resource.uri()),
            OAuthDiscoveryTimeout::Capped(Duration::from_secs(10)),
        )?;
        let response = adapter.execute_request(
            oauth2::http::Request::builder()
                .method("GET")
                .uri(format!("{}/metadata", resource.uri()))
                .body(Vec::new())?,
            OAuthHttpRedirectPolicy::Follow,
            Some(Duration::from_secs(10)),
        ).await.map_err(|error| anyhow::anyhow!(error))?;
        assert_eq!(response.status(), oauth2::http::StatusCode::OK);
        resource.verify().await;
        Ok(())
    }

    #[tokio::test]
    async fn credentialed_redirects_are_bounded_to_resource_origin() -> anyhow::Result<()> {
        let resource = MockServer::start().await;
        let destination = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/register"))
            .and(header("x-api-key", "synthetic-secret"))
            .respond_with(ResponseTemplate::new(307).insert_header("location", format!("{}/other", destination.uri())))
            .expect(1)
            .mount(&resource)
            .await;
        Mock::given(method("POST")).and(path("/other"))
            .respond_with(ResponseTemplate::new(201))
            .expect(0)
            .mount(&destination)
            .await;
        let defaults = crate::utils::build_default_headers(
            Some(HashMap::from([("x-api-key".to_string(), "synthetic-secret".to_string())])),
            /*env_http_headers*/ None,
        )?;
        let adapter = OAuthHttpClientAdapter::new_scoped(
            Arc::new(RouteAwareHttpClient::new(HttpClientFactory::new(
                OutboundProxyPolicy::ReqwestDefault,
            ))),
            defaults,
            &format!("{}/mcp", resource.uri()),
            OAuthDiscoveryTimeout::Requested,
        )?;
        let response = adapter.execute_request(
            oauth2::http::Request::builder()
                .method("POST")
                .uri(format!("{}/register", resource.uri()))
                .body(Vec::new())?,
            OAuthHttpRedirectPolicy::Follow,
            Some(Duration::from_secs(10)),
        ).await.map_err(|error| anyhow::anyhow!(error))?;
        assert_eq!(response.status(), oauth2::http::StatusCode::TEMPORARY_REDIRECT);
        destination.verify().await;
        Ok(())
    }
}
