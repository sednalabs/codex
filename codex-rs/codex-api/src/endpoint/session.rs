use crate::auth::SharedAuthProvider;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::WithStatus;
use crate::telemetry::run_with_attempt_telemetry;
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
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tracing::instrument;

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

/// Complete immutable setup for one credential-bearing application attempt.
///
/// The transport, provider, frozen auth implementation, and one-use initiation must be renewed as
/// one bundle. Keeping them in this type prevents a retry from combining a renewed authority guard
/// with the route, provider URL, provider headers, query parameters, or auth captured by the
/// initial attempt.
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

type ProviderRequestAttemptFuture<T: HttpTransport> =
    Pin<Box<dyn Future<Output = Result<ProviderRequestAttempt<T>, TransportError>> + Send>>;

/// Produces one independently resolved, immutable setup for each application attempt.
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

    #[instrument(
        name = "endpoint_session.execute_with",
        level = "info",
        skip_all,
        fields(http.method = %method, api.path = path)
    )]
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
                    let mut req =
                        Self::make_request(&provider, &method, path, &extra_headers, body.as_ref());
                    configure(&mut req);
                    let req = match auth.apply_auth(req).await {
                        Ok(req) => req,
                        Err(err) => {
                            if let Some(initiation) = initiation {
                                initiation.cancel();
                            }
                            return Err(TransportError::from(err));
                        }
                    };
                    let claim = initiation
                        .map(|initiation| initiation.claim())
                        .transpose()
                        .map_err(TransportError::Build)?;
                    let transport = attempt_transport.as_ref().unwrap_or(static_transport);
                    let response = transport.execute(req);
                    if let Some(claim) = claim {
                        claim.acknowledge();
                    }
                    response
                        .await
                        .map(|response| ResponseWithAuth { response, auth })
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
    ) -> Result<StreamResponse, ApiError>
    where
        C: Fn(&mut Request),
    {
        // Prepare the encoded body and any derived content headers once. The provider and auth
        // remain attempt-local so credential/provider transitions still get a fresh request on
        // every retry, while the immutable wire bytes are shared by each request clone.
        let (prepared_body, prepared_content_type, prepared_content_encoding) = match body
            .map(RequestBody::EncodedJson)
        {
            Some(body) => {
                let mut request =
                    Self::make_request(&self.provider, &method, path, &extra_headers, Some(&body));
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
                    let mut req =
                        Self::make_request(&provider, &method, path, &extra_headers, /*body*/ None);
                    configure(&mut req);
                    if let Some(body) = prepared_body.clone() {
                        req.body = Some(body);
                        // `prepared_body` already contains the exact bytes and the final body
                        // encoding. Do not prepare it again: doing so would discard trace bytes
                        // and, more importantly, could allocate a distinct retry body.
                        req.compression = RequestCompression::None;
                        match prepared_content_type.as_ref() {
                            Some(value) => {
                                req.headers
                                    .insert(http::header::CONTENT_TYPE, value.clone());
                            }
                            None => {
                                req.headers.remove(http::header::CONTENT_TYPE);
                            }
                        }
                        match prepared_content_encoding.as_ref() {
                            Some(value) => {
                                req.headers
                                    .insert(http::header::CONTENT_ENCODING, value.clone());
                            }
                            None => {
                                req.headers.remove(http::header::CONTENT_ENCODING);
                            }
                        }
                    } else {
                        req = match req.into_prepared() {
                            Ok(req) => req,
                            Err(err) => {
                                if let Some(initiation) = initiation {
                                    initiation.cancel();
                                }
                                return Err(TransportError::Build(err));
                            }
                        };
                    }
                    let req = match auth.apply_auth(req).await {
                        Ok(req) => req,
                        Err(err) => {
                            if let Some(initiation) = initiation {
                                initiation.cancel();
                            }
                            return Err(TransportError::from(err));
                        }
                    };
                    let claim = initiation
                        .map(|initiation| initiation.claim())
                        .transpose()
                        .map_err(TransportError::Build)?;
                    let transport = attempt_transport.as_ref().unwrap_or(static_transport);
                    let response = transport.stream(req);
                    if let Some(claim) = claim {
                        claim.acknowledge();
                    }
                    response.await
                }
            },
        )
        .await?;

        Ok(stream)
    }
}
