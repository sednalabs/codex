use crate::auth::SharedAuthProvider;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::run_with_attempt_telemetry;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::RequestBody;
use codex_client::RequestInitiation;
use codex_client::RequestTelemetry;
use codex_client::Response;
use codex_client::StreamResponse;
use codex_client::TransportError;
use http::HeaderMap;
use http::Method;
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
    request_attempt_factory: Option<ProviderRequestAttemptFactory>,
}

/// Complete immutable setup for one credential-bearing application attempt.
///
/// The provider, frozen auth implementation, and one-use initiation must be renewed as one bundle.
/// Keeping them in this type prevents a retry from combining a renewed authority guard with the
/// provider URL, provider headers, query parameters, or auth captured by the initial attempt.
pub struct ProviderRequestAttempt {
    provider: Provider,
    auth: SharedAuthProvider,
    _authority: Box<dyn Send + 'static>,
    initiation: RequestInitiation,
}

impl ProviderRequestAttempt {
    pub fn new<A>(
        provider: Provider,
        auth: SharedAuthProvider,
        authority: A,
        initiation: RequestInitiation,
    ) -> Self
    where
        A: Send + 'static,
    {
        Self {
            provider,
            auth,
            _authority: Box::new(authority),
            initiation,
        }
    }
}

type ProviderRequestAttemptFuture =
    Pin<Box<dyn Future<Output = Result<ProviderRequestAttempt, TransportError>> + Send>>;

/// Produces one independently resolved, immutable setup for each application attempt.
#[derive(Clone)]
pub struct ProviderRequestAttemptFactory {
    create: Arc<dyn Fn() -> ProviderRequestAttemptFuture + Send + Sync>,
}

impl std::fmt::Debug for ProviderRequestAttemptFactory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProviderRequestAttemptFactory(<redacted>)")
    }
}

impl ProviderRequestAttemptFactory {
    pub fn new<F, Fut>(create: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<ProviderRequestAttempt, TransportError>> + Send + 'static,
    {
        Self {
            create: Arc::new(move || Box::pin(create())),
        }
    }

    async fn create(&self) -> Result<ProviderRequestAttempt, TransportError> {
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
        factory: Option<ProviderRequestAttemptFactory>,
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
        let body = body.map(RequestBody::Json);
        let response = run_with_attempt_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            |_| {
                let transport = &self.transport;
                let request_attempt_factory = self.request_attempt_factory.clone();
                let static_provider = self.provider.clone();
                let static_auth = self.auth.clone();
                let method = method.clone();
                let extra_headers = extra_headers.clone();
                let body = body.clone();
                let configure = &configure;
                async move {
                    let (provider, auth, initiation) = match request_attempt_factory {
                        Some(factory) => {
                            let attempt = factory.create().await?;
                            (attempt.provider, attempt.auth, Some(attempt.initiation))
                        }
                        None => (static_provider, static_auth, None),
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
                    let response = transport.execute(req);
                    if let Some(claim) = claim {
                        claim.acknowledge();
                    }
                    response.await
                }
            },
        )
        .await?;

        Ok(response)
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
        let body = body.map(RequestBody::EncodedJson);
        let stream = run_with_attempt_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            |_| {
                let transport = &self.transport;
                let request_attempt_factory = self.request_attempt_factory.clone();
                let static_provider = self.provider.clone();
                let static_auth = self.auth.clone();
                let method = method.clone();
                let extra_headers = extra_headers.clone();
                let body = body.clone();
                let configure = &configure;
                async move {
                    let (provider, auth, initiation) = match request_attempt_factory {
                        Some(factory) => {
                            let attempt = factory.create().await?;
                            (attempt.provider, attempt.auth, Some(attempt.initiation))
                        }
                        None => (static_provider, static_auth, None),
                    };
                    let mut req =
                        Self::make_request(&provider, &method, path, &extra_headers, body.as_ref());
                    configure(&mut req);
                    let req = match req.into_prepared() {
                        Ok(req) => req,
                        Err(err) => {
                            if let Some(initiation) = initiation {
                                initiation.cancel();
                            }
                            return Err(TransportError::Build(err));
                        }
                    };
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
