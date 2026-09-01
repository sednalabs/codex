use crate::auth::SharedAuthProvider;
use crate::common::CompactionInput;
use crate::endpoint::session::EndpointSession;
use crate::error::ApiError;
use crate::provider::Provider;
use codex_client::HttpTransport;
use codex_client::RequestInitiation;
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

    pub fn with_request_initiation(self, initiation: Option<RequestInitiation>) -> Self {
        Self {
            session: self.session.with_request_initiation(initiation),
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
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tokio::sync::Notify;
    use tokio::sync::RwLock;

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
        .with_request_initiation(Some(RequestInitiation::new(authority)));

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
    struct FailingTransport {
        attempts: Arc<AtomicUsize>,
    }

    impl HttpTransport for FailingTransport {
        async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(TransportError::Network("send failed".to_string()))
        }

        async fn stream(&self, _req: Request) -> Result<StreamResponse, TransportError> {
            Err(TransportError::Build("stream should not run".to_string()))
        }
    }

    #[tokio::test]
    async fn credential_bound_compaction_does_not_retry_with_stale_authority() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let client = CompactClient::new(
            FailingTransport {
                attempts: Arc::clone(&attempts),
            },
            test_provider(),
            Arc::new(NoAuth),
        )
        .with_request_initiation(Some(RequestInitiation::new(())));

        let error = client
            .compact(
                serde_json::json!({"model": "test"}),
                HeaderMap::new(),
                Duration::from_secs(5),
                None,
            )
            .await
            .expect_err("transport failure should surface");
        assert!(matches!(
            error,
            ApiError::Transport(TransportError::Network(_))
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
