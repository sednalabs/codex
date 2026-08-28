use crate::auth::SharedAuthProvider;
use crate::error::ApiError;
use crate::provider::Provider;
use crate::telemetry::RequestAttemptObserver;
use crate::telemetry::run_with_request_telemetry;
use crate::telemetry::run_with_request_telemetry_observed;
use codex_client::EncodedJsonBody;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_client::RequestBody;
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
}

impl<T: HttpTransport> EndpointSession<T> {
    pub(crate) fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            transport,
            provider,
            auth,
            request_telemetry: None,
        }
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

        let response = run_with_request_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.execute(req).await
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

        let stream = run_with_request_telemetry(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                async move {
                    let req = auth.apply_auth(req).await.map_err(TransportError::from)?;
                    transport.stream(req).await
                }
            },
        )
        .await?;

        Ok(stream)
    }

    pub(crate) async fn stream_encoded_json_with_observer<C>(
        &self,
        method: Method,
        path: &str,
        extra_headers: HeaderMap,
        body: Option<EncodedJsonBody>,
        configure: C,
        observer: Arc<dyn RequestAttemptObserver>,
    ) -> Result<StreamResponse, ApiError>
    where
        C: Fn(&mut Request),
    {
        let body = body.map(RequestBody::EncodedJson);
        let mut request = self.make_request(&method, path, &extra_headers, body.as_ref());
        configure(&mut request);
        let request = request.into_prepared().map_err(TransportError::Build)?;
        let make_request = || request.clone();

        let stream = run_with_request_telemetry_observed(
            self.provider.retry.to_policy(),
            self.request_telemetry.clone(),
            make_request,
            |req| {
                let auth = self.auth.clone();
                let transport = &self.transport;
                let observer = Arc::clone(&observer);
                async move {
                    let req = match auth.apply_auth(req).await {
                        Ok(req) => req,
                        Err(error) => {
                            let error = TransportError::from(error);
                            observer.on_request_admission_failure(&error.to_string());
                            return Err(error);
                        }
                    };
                    observer.on_request_open();
                    let result = transport.stream(req).await;
                    if let Err(error) = &result {
                        observer.on_request_failure(
                            &error.to_string(),
                            matches!(error, TransportError::Http { .. }),
                            matches!(
                                error,
                                TransportError::Http {
                                    status,
                                    body: Some(body),
                                    ..
                                } if status.as_u16() == 429
                                    && crate::telemetry::http_body_is_usage_limit(body)
                            ),
                        );
                    }
                    result
                }
            },
        )
        .await?;

        Ok(stream)
    }
}
