use crate::auth::SharedAuthProvider;
use crate::common::CompactionInput;
use crate::endpoint::session::EndpointSession;
use crate::endpoint::session::RequestInitiationFactory;
use crate::error::ApiError;
use crate::provider::Provider;
use codex_client::HttpTransport;
use codex_client::RequestTelemetry;
use codex_protocol::models::ResponseItem;
use http::HeaderMap;
use http::Method;
use serde::Deserialize;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

const X_CODEX_TURN_STATE_HEADER: &str = "x-codex-turn-state";

pub struct CompactClient<T: HttpTransport> {
    session: EndpointSession<T>,
}

impl<T: HttpTransport> CompactClient<T> {
    pub fn new(transport: T, provider: Provider, auth: SharedAuthProvider) -> Self {
        Self {
            session: EndpointSession::new(transport, provider, auth),
        }
    }

    pub fn with_telemetry(self, request: Option<Arc<dyn RequestTelemetry>>) -> Self {
        Self {
            session: self.session.with_request_telemetry(request),
        }
    }

    pub fn with_request_initiation_factory(
        self,
        factory: Option<RequestInitiationFactory>,
    ) -> Self {
        Self {
            session: self.session.with_request_initiation_factory(factory),
        }
    }

    fn path() -> &'static str {
        "responses/compact"
    }

    pub async fn compact(
        &self,
        body: serde_json::Value,
        extra_headers: HeaderMap,
        request_timeout: Duration,
        turn_state: Option<&OnceLock<String>>,
    ) -> Result<Vec<ResponseItem>, ApiError> {
        let resp = self
            .session
            .execute_with(
                Method::POST,
                Self::path(),
                extra_headers,
                Some(body),
                |req| {
                    req.timeout = Some(request_timeout);
                },
            )
            .await?;
        if let Some(turn_state) = turn_state
            && let Some(header_value) = resp
                .headers
                .get(X_CODEX_TURN_STATE_HEADER)
                .and_then(|value| value.to_str().ok())
        {
            let _ = turn_state.set(header_value.to_string());
        }
        let parsed: CompactHistoryResponse =
            serde_json::from_slice(&resp.body).map_err(|e| ApiError::Stream(e.to_string()))?;
        Ok(parsed.output)
    }

    pub async fn compact_input(
        &self,
        input: &CompactionInput<'_>,
        extra_headers: HeaderMap,
        request_timeout: Duration,
        turn_state: Option<&OnceLock<String>>,
    ) -> Result<Vec<ResponseItem>, ApiError> {
        let body = serde_json::to_value(input)
            .map_err(|e| ApiError::Stream(format!("failed to encode compaction input: {e}")))?;
        self.compact(body, extra_headers, request_timeout, turn_state)
            .await
    }
}

#[derive(Debug, Deserialize)]
struct CompactHistoryResponse {
    output: Vec<ResponseItem>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthProvider;
    use crate::provider::RetryConfig;
    use codex_client::Request;
    use codex_client::RequestInitiation;
    use codex_client::Response;
    use codex_client::StreamResponse;
    use codex_client::TransportError;
    use http::StatusCode;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tokio::sync::Notify;
    use tokio::sync::RwLock;

    fn one_initiation_factory(initiation: RequestInitiation) -> RequestInitiationFactory {
        let initiation = Arc::new(Mutex::new(Some(initiation)));
        RequestInitiationFactory::new(move || {
            let initiation = initiation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            async move {
                initiation.ok_or_else(|| {
                    TransportError::Build("test initiation already issued".to_string())
                })
            }
        })
    }

    #[derive(Clone, Default)]
    struct DummyTransport;

    impl HttpTransport for DummyTransport {
        async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
            Err(TransportError::Build("execute should not run".to_string()))
        }

        async fn stream(&self, _req: Request) -> Result<StreamResponse, TransportError> {
            Err(TransportError::Build("stream should not run".to_string()))
        }
    }

    #[test]
    fn path_is_responses_compact() {
        assert_eq!(CompactClient::<DummyTransport>::path(), "responses/compact");
    }

    #[derive(Clone, Default)]
    struct NoAuth;

    impl AuthProvider for NoAuth {
        fn add_auth_headers(&self, _headers: &mut HeaderMap) {}
    }

    #[derive(Clone)]
    struct BarrierTransport {
        started: Arc<Notify>,
        release_response: Arc<Notify>,
    }

    impl HttpTransport for BarrierTransport {
        async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
            self.started.notify_one();
            self.release_response.notified().await;
            Ok(Response {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: br#"{"output":[]}"#.as_slice().into(),
            })
        }

        async fn stream(&self, _req: Request) -> Result<StreamResponse, TransportError> {
            Err(TransportError::Build("stream should not run".to_string()))
        }
    }

    fn test_provider() -> Provider {
        Provider {
            name: "test".to_string(),
            base_url: "https://example.test/v1".to_string(),
            query_params: None,
            headers: HeaderMap::new(),
            retry: RetryConfig {
                max_attempts: 3,
                base_delay: Duration::ZERO,
                retry_429: true,
                retry_5xx: true,
                retry_transport: true,
            },
            stream_idle_timeout: Duration::from_secs(1),
        }
    }

    #[tokio::test]
    async fn compaction_releases_authority_at_transport_acceptance_before_response() {
        let gate = Arc::new(RwLock::new(()));
        let authority = Arc::clone(&gate).read_owned().await;
        let started = Arc::new(Notify::new());
        let release_response = Arc::new(Notify::new());
        let client = CompactClient::new(
            BarrierTransport {
                started: Arc::clone(&started),
                release_response: Arc::clone(&release_response),
            },
            test_provider(),
            Arc::new(NoAuth),
        )
        .with_request_initiation_factory(Some(one_initiation_factory(
            RequestInitiation::new(authority),
        )));

        let request = tokio::spawn(async move {
            client
                .compact(
                    serde_json::json!({"model": "test"}),
                    HeaderMap::new(),
                    Duration::from_secs(5),
                    None,
                )
                .await
        });
        started.notified().await;
        let transition = tokio::time::timeout(Duration::from_secs(1), gate.write())
            .await
            .expect("transport acceptance should release request authority");
        assert!(
            !request.is_finished(),
            "response must remain pending after authority is released"
        );
        drop(transition);
        release_response.notify_one();
        assert!(request.await.expect("request task").is_ok());
    }

    #[derive(Clone)]
    struct RetryThenSuccessTransport {
        attempts: Arc<AtomicUsize>,
    }

    impl HttpTransport for RetryThenSuccessTransport {
        async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if attempt == 0 {
                return Err(TransportError::Network("send failed".to_string()));
            }
            Ok(Response {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: br#"{"output":[]}"#.as_slice().into(),
            })
        }

        async fn stream(&self, _req: Request) -> Result<StreamResponse, TransportError> {
            Err(TransportError::Build("stream should not run".to_string()))
        }
    }

    struct AttemptAuthority(Arc<AtomicUsize>);

    impl Drop for AttemptAuthority {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn same_authority_retry_uses_one_fresh_initiation_per_wire_attempt() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let issued = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let factory = RequestInitiationFactory::new({
            let issued = Arc::clone(&issued);
            let released = Arc::clone(&released);
            move || {
                issued.fetch_add(1, Ordering::SeqCst);
                let initiation = RequestInitiation::new(AttemptAuthority(Arc::clone(&released)));
                async move { Ok(initiation) }
            }
        });
        let client = CompactClient::new(
            RetryThenSuccessTransport {
                attempts: Arc::clone(&attempts),
            },
            test_provider(),
            Arc::new(NoAuth),
        )
        .with_request_initiation_factory(Some(factory));

        client
            .compact(
                serde_json::json!({"model": "test"}),
                HeaderMap::new(),
                Duration::from_secs(5),
                None,
            )
            .await
            .expect("second application attempt should succeed");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(issued.load(Ordering::SeqCst), 2);
        assert_eq!(released.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn authority_change_before_retry_sends_no_second_wire_attempt() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let factory = RequestInitiationFactory::new({
            let factory_calls = Arc::clone(&factory_calls);
            move || {
                let call = factory_calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        Ok(RequestInitiation::new(()))
                    } else {
                        Err(TransportError::AutomaticTurnContextChanged)
                    }
                }
            }
        });
        let client = CompactClient::new(
            RetryThenSuccessTransport {
                attempts: Arc::clone(&attempts),
            },
            test_provider(),
            Arc::new(NoAuth),
        )
        .with_request_initiation_factory(Some(factory));

        let error = client
            .compact(
                serde_json::json!({"model": "test"}),
                HeaderMap::new(),
                Duration::from_secs(5),
                None,
            )
            .await
            .expect_err("changed authority should stop the visible retry");
        assert!(matches!(
            error,
            ApiError::Transport(TransportError::AutomaticTurnContextChanged)
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(factory_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn ordinary_compaction_preserves_application_retry_and_backoff() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut provider = test_provider();
        provider.retry.base_delay = Duration::from_millis(20);
        let client = CompactClient::new(
            RetryThenSuccessTransport {
                attempts: Arc::clone(&attempts),
            },
            provider,
            Arc::new(NoAuth),
        );
        let started = tokio::time::Instant::now();

        client
            .compact(
                serde_json::json!({"model": "test"}),
                HeaderMap::new(),
                Duration::from_secs(5),
                None,
            )
            .await
            .expect("ordinary retry should succeed");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(
            started.elapsed() >= Duration::from_millis(15),
            "configured backoff should remain observable"
        );
    }
}
