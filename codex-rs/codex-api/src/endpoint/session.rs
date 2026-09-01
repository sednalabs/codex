use crate::auth::SharedAuthProvider;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::run_with_request_telemetry;
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
use std::sync::Arc;
use tracing::instrument;

pub(crate) struct EndpointSession<T: HttpTransport> {
    transport: T,
    provider: Provider,
    auth: SharedAuthProvider,
    request_telemetry: Option<Arc<dyn RequestTelemetry>>,
    request_initiation: Option<RequestInitiation>,
}

impl<T: HttpTransport> EndpointSession<T> {
    pub(crate) fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            transport,
            provider,
            auth,
            request_telemetry: None,
            request_initiation: None,
        }
    }

    pub(crate) fn with_request_initiation(mut self, initiation: Option<RequestInitiation>) -> Self {
        self.request_initiation = initiation;
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
        &self,
        method: &Method,
        path: &str,
        extra_headers: &HeaderMap,
        body: Option<&RequestBody>,
    ) -> Request {
        let mut req = self.provider.build_request(method.clone(), path);
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
        let make_request = || {
            let mut req = self.make_request(&method, path, &extra_headers, body.as_ref());
            configure(&mut req);
            req
        };

        let mut retry_policy = self.provider.retry.to_policy();
        if self.request_initiation.is_some() {
            retry_policy.max_attempts = 0;
        }
        let response = run_with_request_telemetry(
            retry_policy,
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                let initiation = self.request_initiation.clone();
                async move {
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
        let mut request = self.make_request(&method, path, &extra_headers, body.as_ref());
        configure(&mut request);
        let request = request.into_prepared().map_err(TransportError::Build)?;
        let make_request = || request.clone();

        let mut retry_policy = self.provider.retry.to_policy();
        if self.request_initiation.is_some() {
            retry_policy.max_attempts = 0;
        }
        let stream = run_with_request_telemetry(
            retry_policy,
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                let initiation = self.request_initiation.clone();
                async move {
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
