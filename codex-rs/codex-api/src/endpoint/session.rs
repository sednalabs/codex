use crate::auth::SharedAuthProvider;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::WithStatus;
use crate::telemetry::run_with_attempt_telemetry;
use chrono::SecondsFormat;
use chrono::Utc;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::RequestBody;
use codex_client::RequestCompression;
use codex_client::RequestInitiation;
use codex_client::RequestTelemetry;
use codex_client::Response;
use codex_client::StreamResponse;
use codex_client::TransportError;
use http::HeaderMap;
use http::Method;
use http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::instrument;

struct StartedStreamResponse {
    stream: StreamResponse,
    started_at: String,
}

impl WithStatus for StartedStreamResponse {
    fn status(&self) -> http::StatusCode {
        self.stream.status
    }
}

pub(crate) struct EndpointSession<T: HttpTransport> {
    transport: T,
    provider: Provider,
    auth: SharedAuthProvider,
    request_telemetry: Option<Arc<dyn RequestTelemetry>>,
    request_attempt_factory: Option<ProviderRequestAttemptFactory<T>>,
}

struct ResponseWithAuth {
    response: Response,
    auth: SharedAuthProvider,
}

impl WithStatus for ResponseWithAuth {
    fn status(&self) -> StatusCode {
        self.response.status
    }
}

/// The transport, provider, frozen credentials and one-use request authority that belong to one
/// application retry attempt. Keeping these together prevents mixing a refreshed auth guard with
/// a stale route or provider snapshot.
pub struct ProviderRequestAttempt<T: HttpTransport> {
    transport: T,
    provider: Provider,
    auth: SharedAuthProvider,
    _authority: Box<dyn Send + 'static>,
    initiation: RequestInitiation,
}

impl<T: HttpTransport> ProviderRequestAttempt<T> {
    pub fn new<A>(
        transport: T,
        provider: Provider,
        auth: SharedAuthProvider,
        authority: A,
        initiation: RequestInitiation,
    ) -> Self
    where
        A: Send + 'static,
    {
        Self {
            transport,
            provider,
            auth,
            _authority: Box::new(authority),
            initiation,
        }
    }
}

type ProviderRequestAttemptFuture<T> =
    Pin<Box<dyn Future<Output = Result<ProviderRequestAttempt<T>, TransportError>> + Send>>;

/// Creates one complete auth/provider/transport snapshot per application attempt.
pub struct ProviderRequestAttemptFactory<T: HttpTransport> {
    create: Arc<dyn Fn() -> ProviderRequestAttemptFuture<T> + Send + Sync>,
}

impl<T: HttpTransport> Clone for ProviderRequestAttemptFactory<T> {
    fn clone(&self) -> Self {
        Self {
            create: Arc::clone(&self.create),
        }
    }
}

impl<T: HttpTransport> std::fmt::Debug for ProviderRequestAttemptFactory<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProviderRequestAttemptFactory(<redacted>)")
    }
}

impl<T: HttpTransport> ProviderRequestAttemptFactory<T> {
    pub fn new<F, Fut>(create: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ProviderRequestAttempt<T>, TransportError>> + Send + 'static,
    {
        Self {
            create: Arc::new(move || Box::pin(create())),
        }
    }

    async fn create(&self) -> Result<ProviderRequestAttempt<T>, TransportError> {
        (self.create)().await
    }
}

impl<T: HttpTransport> EndpointSession<T> {
    pub(crate) fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            transport,
            provider,
            auth,
            request_telemetry: None,
            request_attempt_factory: None,
        }
    }

    pub(crate) fn with_request_attempt_factory(
        mut self,
        factory: Option<ProviderRequestAttemptFactory<T>>,
    ) -> Self {
        self.request_attempt_factory = factory;
        self
    }

    pub(crate) fn with_request_telemetry(
        mut self,
        request: Option<Arc<dyn RequestTelemetry>>,
    ) -> Self {
        self.request_telemetry = request;
        self
    }

    pub(crate) fn provider(&self) -> &Provider {
        &self.provider
    }

    fn make_request(
        provider: &Provider,
        method: &Method,
        path: &str,
        extra_headers: &HeaderMap,
        body: Option<&RequestBody>,
    ) -> Request {
        let mut req = provider.build_request(method.clone(), path);
        req.headers.extend(extra_headers.clone());
        if let Some(body) = body {
            req.body = Some(body.clone());
        }
        req
    }

    pub(crate) async fn execute(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
    ) -> Result<Response, ApiError> {
        self.execute_with(method, path, extra_headers, body, |_| {})
            .await
    }

    #[instrument(
        name = "endpoint_session.execute_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn execute_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
        configure: C,
    ) -> Result<Response, ApiError>
    where
        C: Fn(&mut Request),
    {
        self.execute_with_auth_inner(method, path, extra_headers, body, configure)
            .await
            .map(|(response, _auth)| response)
    }

    pub(crate) async fn execute_with_auth<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
        configure: C,
    ) -> Result<(Response, SharedAuthProvider), ApiError>
    where
        C: Fn(&mut Request),
    {
        self.execute_with_auth_inner(method, path, extra_headers, body, configure)
            .await
    }

    async fn execute_with_auth_inner<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<Value>,
        configure: C,
    ) -> Result<(Response, SharedAuthProvider), ApiError>
    where
        C: Fn(&mut Request),
    {
        let body = body.map(RequestBody::Json);
        let response_with_auth = run_with_attempt_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            |_| {
                let static_transport = &self.transport;
                let request_attempt_factory = self.request_attempt_factory.clone();
                let static_provider = self.provider.clone();
                let static_auth = self.auth.clone();
                let method = method.clone();
                let extra_headers = extra_headers.clone();
                let body = body.clone();
                let configure = &configure;
                async move {
                    let (attempt_transport, provider, auth, initiation) =
                        match request_attempt_factory {
                            Some(factory) => {
                                let attempt = factory.create().await?;
                                (
                                    Some(attempt.transport),
                                    attempt.provider,
                                    attempt.auth,
                                    Some(attempt.initiation),
                                )
                            }
                            None => (None, static_provider, static_auth, None),
                        };
                    let mut request =
                        Self::make_request(&provider, &method, path, &extra_headers, body.as_ref());
                    configure(&mut request);
                    let request = match auth.apply_auth(request).await {
                        Ok(request) => request,
                        Err(error) => {
                            if let Some(initiation) = initiation {
                                initiation.cancel();
                            }
                            return Err(TransportError::from(error));
                        }
                    };
                    let claim = initiation
                        .map(|initiation| initiation.claim())
                        .transpose()
                        .map_err(TransportError::Build)?;
                    let transport = attempt_transport.as_ref().unwrap_or(static_transport);
                    let response = transport.execute(request);
                    if let Some(claim) = claim {
                        claim.acknowledge();
                    }
                    response.await.map(|response| ResponseWithAuth { response, auth })
                }
            },
        )
        .await?;

        Ok((response_with_auth.response, response_with_auth.auth))
    }

    #[instrument(
        name = "endpoint_session.stream_encoded_json_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
    pub(crate) async fn stream_encoded_json_with<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<EncodedJsonBody>,
        configure: C,
    ) -> Result<(StreamResponse, String), ApiError>
    where
        C: Fn(&mut Request),
    {
        let (prepared_body, prepared_content_type, prepared_content_encoding) =
            match body.map(RequestBody::EncodedJson) {
                Some(body) => {
                    let mut request = Self::make_request(
                        &self.provider,
                        &method,
                        path,
                        &extra_headers,
                        Some(&body),
                    );
                    configure(&mut request);
                    let request = request.into_prepared().map_err(TransportError::Build)?;
                    (
                        request.body,
                        request.headers.get(http::header::CONTENT_TYPE).cloned(),
                        request.headers.get(http::header::CONTENT_ENCODING).cloned(),
                    )
                }
                None => (None, None, None),
            };

        let stream = run_with_attempt_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            |_| {
                let static_transport = &self.transport;
                let request_attempt_factory = self.request_attempt_factory.clone();
                let static_provider = self.provider.clone();
                let static_auth = self.auth.clone();
                let method = method.clone();
                let extra_headers = extra_headers.clone();
                let prepared_body = prepared_body.clone();
                let prepared_content_type = prepared_content_type.clone();
                let prepared_content_encoding = prepared_content_encoding.clone();
                let configure = &configure;
                async move {
                    let (attempt_transport, provider, auth, initiation) =
                        match request_attempt_factory {
                            Some(factory) => {
                                let attempt = factory.create().await?;
                                (
                                    Some(attempt.transport),
                                    attempt.provider,
                                    attempt.auth,
                                    Some(attempt.initiation),
                                )
                            }
                            None => (None, static_provider, static_auth, None),
                        };
                    let mut request = Self::make_request(
                        &provider,
                        &method,
                        path,
                        &extra_headers,
                        /*body*/ None,
                    );
                    configure(&mut request);
                    if let Some(body) = prepared_body.clone() {
                        request.body = Some(body);
                        request.compression = RequestCompression::None;
                        match prepared_content_type.as_ref() {
                            Some(value) => {
                                request.headers.insert(http::header::CONTENT_TYPE, value.clone());
                            }
                            None => {
                                request.headers.remove(http::header::CONTENT_TYPE);
                            }
                        }
                        match prepared_content_encoding.as_ref() {
                            Some(value) => {
                                request
                                    .headers
                                    .insert(http::header::CONTENT_ENCODING, value.clone());
                            }
                            None => {
                                request.headers.remove(http::header::CONTENT_ENCODING);
                            }
                        }
                    } else {
                        request = match request.into_prepared() {
                            Ok(request) => request,
                            Err(error) => {
                                if let Some(initiation) = initiation {
                                    initiation.cancel();
                                }
                                return Err(TransportError::Build(error));
                            }
                        };
                    }
                    let request = match auth.apply_auth(request).await {
                        Ok(request) => request,
                        Err(error) => {
                            if let Some(initiation) = initiation {
                                initiation.cancel();
                            }
                            return Err(TransportError::from(error));
                        }
                    };
                    let claim = initiation
                        .map(|initiation| initiation.claim())
                        .transpose()
                        .map_err(TransportError::Build)?;
                    let started_at = Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true);
                    let transport = attempt_transport.as_ref().unwrap_or(static_transport);
                    let response = transport.stream(request);
                    if let Some(claim) = claim {
                        claim.acknowledge();
                    }
                    response
                        .await
                        .map(|stream| StartedStreamResponse { stream, started_at })
                }
            },
        )
        .await;

        let stream = match stream {
            Ok(stream) => stream,
            Err(error) if is_transient_availability_response(&error) => {
                return Err(ApiError::TransientAvailability(error));
            }
            Err(error) => return Err(ApiError::Transport(error)),
        };

        Ok((stream.stream, stream.started_at))
    }
}

#[derive(Deserialize)]
struct AvailabilityErrorEnvelope {
    error: AvailabilityError,
}

#[derive(Deserialize)]
struct AvailabilityError {
    #[serde(rename = "type")]
    error_type: String,
    code: String,
}

fn is_transient_availability_response(error: &TransportError) -> bool {
    let TransportError::Http {
        status,
        headers,
        body: Some(body),
        ..
    } = error
    else {
        return false;
    };

    if *status != http::StatusCode::SERVICE_UNAVAILABLE
        || headers.as_ref().is_some_and(|headers| {
            headers.contains_key("x-openai-authorization-error")
                || headers.contains_key("x-error-json")
        })
    {
        return false;
    }

    serde_json::from_str::<AvailabilityErrorEnvelope>(body).is_ok_and(|envelope| {
        envelope.error.error_type == "rate_limit_error" && envelope.error.code == "slow_down"
    })
}
