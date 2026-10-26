use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use codex_exec_server::ExecServerError;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpRequestParams;
use codex_exec_server::HttpRequestResponse;
use codex_exec_server::HttpResponseBodyStream;
use futures::future::BoxFuture;
use http::HeaderMap;
use http::header::AUTHORIZATION;
use serde_json::json;

use super::*;

struct Reply {
    status: u16,
    body: Vec<u8>,
}

#[derive(Default)]
struct TestHttpClient {
    replies: Mutex<VecDeque<Reply>>,
    requests: Mutex<Vec<HttpRequestParams>>,
    pending: bool,
}

impl TestHttpClient {
    fn with_replies(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            pending: false,
        }
    }

    fn pending() -> Self {
        Self {
            replies: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            pending: true,
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl HttpClient for TestHttpClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        self.requests.lock().unwrap().push(params);
        let reply = self.replies.lock().unwrap().pop_front();
        let pending = self.pending;
        Box::pin(async move {
            if pending {
                return std::future::pending().await;
            }
            let reply = reply.ok_or_else(|| {
                ExecServerError::HttpRequest("unexpected test HTTP request".to_string())
            })?;
            Ok(HttpRequestResponse {
                status: reply.status,
                headers: Vec::new(),
                body: reply.body.into(),
            })
        })
    }

    fn http_request_stream(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        self.requests.lock().unwrap().push(params);
        let reply = self.replies.lock().unwrap().pop_front();
        let pending = self.pending;
        Box::pin(async move {
            if pending {
                return std::future::pending().await;
            }
            let reply = reply.ok_or_else(|| {
                ExecServerError::HttpRequest("unexpected test HTTP request".to_string())
            })?;
            Ok((
                HttpRequestResponse {
                    status: reply.status,
                    headers: Vec::new(),
                    body: Vec::new().into(),
                },
                HttpResponseBodyStream::from_chunks(vec![reply.body]),
            ))
        })
    }
}

fn details(expires_in: u64, interval: u64) -> DeviceAuthorizationResponse {
    DeviceAuthorizationResponse {
        device_code: "synthetic-device-code".to_string(),
        user_code: "SYNTHETIC-CODE".to_string(),
        verification_uri: "https://issuer.test/device".to_string(),
        verification_uri_complete: None,
        expires_in: Some(expires_in),
        interval: Some(interval),
    }
}

fn reply(status: u16, body: serde_json::Value) -> Reply {
    Reply {
        status,
        body: serde_json::to_vec(&body).unwrap(),
    }
}

#[test]
fn oauth_endpoint_headers_keep_resource_credentials_on_same_origin_only() {
    let mut resource_headers = HeaderMap::new();
    resource_headers.insert(AUTHORIZATION, "Bearer resource-secret".parse().unwrap());
    resource_headers.insert("x-resource-secret", "resource-secret".parse().unwrap());

    let same_origin = oauth_endpoint_headers(
        "https://mcp.example/mcp",
        "https://mcp.example/device",
        &resource_headers,
    )
    .expect("same-origin OAuth endpoints may use MCP resource headers");
    assert_eq!(
        same_origin.get(AUTHORIZATION).unwrap(),
        "Bearer resource-secret"
    );
    assert_eq!(
        same_origin.get("x-resource-secret").unwrap(),
        "resource-secret"
    );

    let authorization_server = oauth_endpoint_headers(
        "https://mcp.example/mcp",
        "https://issuer.example/device",
        &resource_headers,
    )
    .expect("authorization-server endpoints must be routable without resource credentials");
    assert!(!authorization_server.contains_key(AUTHORIZATION));
    assert!(!authorization_server.contains_key("x-resource-secret"));
    assert_eq!(
        authorization_server.get(http::header::USER_AGENT).unwrap(),
        MCP_USER_AGENT
    );
}

#[tokio::test]
async fn a_slow_token_endpoint_is_bounded_by_the_device_expiry() {
    let client = Arc::new(TestHttpClient::pending());
    let error = poll_device_token(
        client.as_ref(),
        &HeaderMap::new(),
        "https://issuer.test/token",
        "synthetic-client",
        /*oauth_resource*/ None,
        &details(/*expires_in*/ 1, /*interval*/ 1),
        /*pkce*/ None,
    )
    .await
    .expect_err("the device expiry must bound an in-flight poll");

    assert_eq!(
        error.to_string(),
        "OAuth device code expired before authorization completed"
    );
    assert_eq!(client.request_count(), 1);
}

#[tokio::test]
async fn denial_and_expiration_stop_polling() {
    for (error, expected) in [
        ("access_denied", "OAuth device authorization was denied"),
        (
            "expired_token",
            "OAuth device code expired before authorization completed",
        ),
    ] {
        let client = Arc::new(TestHttpClient::with_replies([reply(
            /*status*/ 400,
            json!({"error": error}),
        )]));
        let actual = poll_device_token(
            client.as_ref(),
            &HeaderMap::new(),
            "https://issuer.test/token",
            "synthetic-client",
            /*oauth_resource*/ None,
            &details(/*expires_in*/ 60, /*interval*/ 1),
            /*pkce*/ None,
        )
        .await
        .expect_err("terminal provider errors must stop device login");

        assert_eq!(actual.to_string(), expected);
        assert_eq!(client.request_count(), 1);
    }
}

#[tokio::test]
async fn slow_down_does_not_extend_the_original_device_deadline() {
    let client = Arc::new(TestHttpClient::with_replies([reply(
        /*status*/ 400,
        json!({"error": "slow_down"}),
    )]));
    let error = poll_device_token(
        client.as_ref(),
        &HeaderMap::new(),
        "https://issuer.test/token",
        "synthetic-client",
        /*oauth_resource*/ None,
        &details(/*expires_in*/ 1, /*interval*/ 1),
        /*pkce*/ None,
    )
    .await
    .expect_err("slow_down must preserve the original deadline");

    assert_eq!(
        error.to_string(),
        "OAuth device code expired before authorization completed"
    );
    assert_eq!(client.request_count(), 1);
}

#[tokio::test]
async fn a_provider_invalid_request_retries_device_authorization_without_pkce() {
    let client = TestHttpClient::with_replies([
        reply(
            /*status*/ 400,
            json!({"error": "invalid_request", "error_description": "PKCE not supported"}),
        ),
        reply(
            /*status*/ 200,
            json!({
                "device_code": "synthetic-device-code",
                "user_code": "SYNTHETIC-CODE",
                "verification_uri": "https://issuer.test/device"
            }),
        ),
    ]);
    let (details, pkce) = request_device_authorization_with_pkce_fallback(
        &client,
        &HeaderMap::new(),
        "https://issuer.test/device",
        "synthetic-client",
        &["read".to_string()],
        /*oauth_resource*/ None,
    )
    .await
    .expect("provider PKCE rejection should have one public-client fallback");

    assert_eq!(details.device_code, "synthetic-device-code");
    assert!(pkce.is_none());
    let requests = client.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let first: BTreeMap<String, String> =
        url::form_urlencoded::parse(requests[0].body.as_ref().unwrap().as_ref())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
    let second: BTreeMap<String, String> =
        url::form_urlencoded::parse(requests[1].body.as_ref().unwrap().as_ref())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
    assert!(first.contains_key("code_challenge"));
    assert_eq!(
        first.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert!(!second.contains_key("code_challenge"));
    assert!(!second.contains_key("code_challenge_method"));
}
