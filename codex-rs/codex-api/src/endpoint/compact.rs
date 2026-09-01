use crate::auth::SharedAuthProvider;
use crate::common::CompactionInput;
use crate::endpoint::session::EndpointSession;
use crate::endpoint::session::ProviderRequestAttemptFactory;
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

    pub fn with_request_attempt_factory(
        self,
        factory: Option<ProviderRequestAttemptFactory<T>>,
    ) -> Self {
        Self {
            session: self.session.with_request_attempt_factory(factory),
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
    use crate::endpoint::session::ProviderRequestAttempt;
    use crate::provider::RetryConfig;
    use codex_client::Request;
    use codex_client::RequestInitiation;
    use codex_client::Response;
    use codex_client::StreamResponse;
    use codex_client::TransportError;
    use http::HeaderValue;
    use http::StatusCode;
    use http::header::AUTHORIZATION;
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tokio::sync::Notify;
    use tokio::sync::RwLock;

    fn one_attempt_factory<T: HttpTransport>(
        transport: T,
        provider: Provider,
        auth: SharedAuthProvider,
        initiation: RequestInitiation,
    ) -> ProviderRequestAttemptFactory<T> {
        let attempt = Arc::new(Mutex::new(Some(ProviderRequestAttempt::new(
            transport,
            provider,
            auth,
            (),
            initiation,
        ))));
        ProviderRequestAttemptFactory::new(move || {
            let attempt = attempt
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            async move {
                attempt.ok_or_else(|| {
                    TransportError::Build("test request attempt already issued".to_string())
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
        .with_request_attempt_factory(Some(one_attempt_factory(
            BarrierTransport {
                started: Arc::clone(&started),
                release_response: Arc::clone(&release_response),
            },
            test_provider(),
            Arc::new(NoAuth),
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

    #[derive(Clone)]
    struct HeaderAuth(&'static str);

    impl AuthProvider for HeaderAuth {
        fn add_auth_headers(&self, headers: &mut HeaderMap) {
            headers.insert(AUTHORIZATION, HeaderValue::from_static(self.0));
        }
    }

    fn named_provider(name: &str, base_url: &str, provider_header: &'static str) -> Provider {
        let mut provider = test_provider();
        provider.name = name.to_string();
        provider.base_url = base_url.to_string();
        provider.query_params = Some(HashMap::from([(
            "deployment".to_string(),
            name.to_string(),
        )]));
        provider
            .headers
            .insert("x-provider", HeaderValue::from_static(provider_header));
        provider
    }

    #[derive(Clone)]
    struct CapturingRetryTransport {
        route: &'static str,
        requests: Arc<Mutex<Vec<(&'static str, Request)>>>,
    }

    impl HttpTransport for CapturingRetryTransport {
        async fn execute(&self, req: Request) -> Result<Response, TransportError> {
            let mut requests = self
                .requests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            requests.push((self.route, req));
            if self.route == "route-a" {
                return Err(TransportError::Network(
                    "first provider unavailable".to_string(),
                ));
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

    #[tokio::test]
    async fn ordinary_retry_rebuilds_the_complete_provider_and_auth_attempt() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let factory = ProviderRequestAttemptFactory::new({
            let factory_calls = Arc::clone(&factory_calls);
            let requests = Arc::clone(&requests);
            move || {
                let call = factory_calls.fetch_add(1, Ordering::SeqCst);
                let requests = Arc::clone(&requests);
                async move {
                    let (route, provider, auth): (&'static str, Provider, SharedAuthProvider) =
                        if call == 0 {
                            (
                                "route-a",
                                named_provider("a", "https://a.example/v1", "provider-a"),
                                Arc::new(HeaderAuth("Bearer auth-a")),
                            )
                        } else {
                            (
                                "route-b",
                                named_provider("b", "https://b.example/v2", "provider-b"),
                                Arc::new(HeaderAuth("Bearer auth-b")),
                            )
                        };
                    Ok(ProviderRequestAttempt::new(
                        CapturingRetryTransport {
                            route,
                            requests: Arc::clone(&requests),
                        },
                        provider,
                        auth,
                        call,
                        RequestInitiation::new(call),
                    ))
                }
            }
        });
        let transport = CapturingRetryTransport {
            route: "stale-route",
            requests: Arc::clone(&requests),
        };
        let mut request_headers = HeaderMap::new();
        request_headers.insert("x-client-request-id", HeaderValue::from_static("request-1"));
        let client = CompactClient::new(
            transport,
            named_provider("stale", "https://stale.example", "provider-stale"),
            Arc::new(HeaderAuth("Bearer auth-stale")),
        )
        .with_request_attempt_factory(Some(factory));

        client
            .compact(
                serde_json::json!({"model": "test", "input": ["same-body"]}),
                request_headers,
                Duration::from_secs(5),
                None,
            )
            .await
            .expect("ordinary retry should succeed with renewed provider B");

        let requests = requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests
                .iter()
                .map(|(_route, request)| request.url.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://a.example/v1/responses/compact?deployment=a",
                "https://b.example/v2/responses/compact?deployment=b",
            ]
        );
        assert_eq!(
            requests
                .iter()
                .map(|(_route, request)| request.headers.get("x-provider"))
                .collect::<Vec<_>>(),
            vec![
                Some(&HeaderValue::from_static("provider-a")),
                Some(&HeaderValue::from_static("provider-b")),
            ]
        );
        assert_eq!(
            requests
                .iter()
                .map(|(_route, request)| request.headers.get(AUTHORIZATION))
                .collect::<Vec<_>>(),
            vec![
                Some(&HeaderValue::from_static("Bearer auth-a")),
                Some(&HeaderValue::from_static("Bearer auth-b")),
            ]
        );
        assert!(requests.iter().all(|(_route, request)| {
            request.headers.get("x-client-request-id")
                == Some(&HeaderValue::from_static("request-1"))
        }));
        assert_eq!(requests[0].1.body, requests[1].1.body);
        assert_eq!(
            requests
                .iter()
                .map(|(route, _request)| *route)
                .collect::<Vec<_>>(),
            vec!["route-a", "route-b"]
        );
        assert_eq!(factory_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn same_authority_retry_uses_one_fresh_initiation_per_wire_attempt() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let issued = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let factory = ProviderRequestAttemptFactory::new({
            let issued = Arc::clone(&issued);
            let released = Arc::clone(&released);
            let attempts = Arc::clone(&attempts);
            move || {
                issued.fetch_add(1, Ordering::SeqCst);
                let initiation = RequestInitiation::new(AttemptAuthority(Arc::clone(&released)));
                let transport = RetryThenSuccessTransport {
                    attempts: Arc::clone(&attempts),
                };
                async move {
                    Ok(ProviderRequestAttempt::new(
                        transport,
                        test_provider(),
                        Arc::new(NoAuth),
                        (),
                        initiation,
                    ))
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
        .with_request_attempt_factory(Some(factory));

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

    #[derive(Clone)]
    struct AlwaysFailTransport {
        attempts: Arc<AtomicUsize>,
    }

    impl HttpTransport for AlwaysFailTransport {
        async fn execute(&self, _req: Request) -> Result<Response, TransportError> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            Err(TransportError::Network("persistent failure".to_string()))
        }

        async fn stream(&self, _req: Request) -> Result<StreamResponse, TransportError> {
            Err(TransportError::Build("stream should not run".to_string()))
        }
    }

    #[tokio::test]
    async fn renewed_attempts_share_one_finite_budget_and_never_reuse_a_token() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let issued = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let factory = ProviderRequestAttemptFactory::new({
            let issued = Arc::clone(&issued);
            let released = Arc::clone(&released);
            let attempts = Arc::clone(&attempts);
            move || {
                let token_id = issued.fetch_add(1, Ordering::SeqCst);
                let initiation =
                    RequestInitiation::new((token_id, AttemptAuthority(Arc::clone(&released))));
                let transport = AlwaysFailTransport {
                    attempts: Arc::clone(&attempts),
                };
                async move {
                    Ok(ProviderRequestAttempt::new(
                        transport,
                        test_provider(),
                        Arc::new(NoAuth),
                        token_id,
                        initiation,
                    ))
                }
            }
        });
        let mut provider = test_provider();
        provider.retry.max_attempts = 2;
        let client = CompactClient::new(
            AlwaysFailTransport {
                attempts: Arc::clone(&attempts),
            },
            provider,
            Arc::new(NoAuth),
        )
        .with_request_attempt_factory(Some(factory));

        let error = client
            .compact(
                serde_json::json!({"model": "test"}),
                HeaderMap::new(),
                Duration::from_secs(5),
                None,
            )
            .await
            .expect_err("the finite retry budget should be exhausted");
        assert!(matches!(
            error,
            ApiError::Transport(TransportError::Network(_))
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
        assert_eq!(issued.load(Ordering::SeqCst), 3);
        assert_eq!(released.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn authority_change_before_retry_sends_no_second_wire_attempt() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::new(AtomicUsize::new(0));
        let factory = ProviderRequestAttemptFactory::new({
            let factory_calls = Arc::clone(&factory_calls);
            let attempts = Arc::clone(&attempts);
            move || {
                let call = factory_calls.fetch_add(1, Ordering::SeqCst);
                let transport = RetryThenSuccessTransport {
                    attempts: Arc::clone(&attempts),
                };
                async move {
                    if call == 0 {
                        Ok(ProviderRequestAttempt::new(
                            transport,
                            test_provider(),
                            Arc::new(NoAuth),
                            (),
                            RequestInitiation::new(()),
                        ))
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
        .with_request_attempt_factory(Some(factory));

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
