use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn details() -> DeviceAuthorizationResponse {
    DeviceAuthorizationResponse {
        device_code: "synthetic-device-code".to_string(),
        user_code: "TEST-CODE".to_string(),
        verification_uri: "https://issuer.test/device".to_string(),
        verification_uri_complete: None,
        expires_in: Some(1),
        interval: Some(1),
    }
}

#[tokio::test]
async fn device_login_expiry_bounds_an_unresponsive_token_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(60))
                .set_body_json(json!({
                    "access_token": "synthetic-access-token",
                    "token_type": "Bearer"
                })),
        )
        .expect(1)
        .mount(&server)
        .await;
    let error = poll_device_token(
        &test_device_client().await,
        &format!("{}/token", server.uri()),
        "configured-client",
        /*oauth_resource*/ None,
        &details(),
        /*pkce*/ None,
    )
    .await
    .expect_err("the device deadline must bound an in-flight request");
    assert_eq!(
        error.to_string(),
        "OAuth device code expired before authorization completed"
    );
}

#[tokio::test]
async fn device_login_denial_and_expiration_stop_without_another_poll() {
    for (provider_code, expected) in [
        ("access_denied", "OAuth device authorization was denied"),
        (
            "expired_token",
            "OAuth device code expired before authorization completed",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": provider_code
            })))
            .expect(1)
            .mount(&server)
            .await;
        let error = poll_device_token(
            &test_device_client().await,
            &format!("{}/token", server.uri()),
            "configured-client",
            /*oauth_resource*/ None,
            &details(),
            /*pkce*/ None,
        )
        .await
        .expect_err("a terminal provider response must stop polling");
        assert_eq!(error.to_string(), expected);
    }
}

#[tokio::test]
async fn device_login_slow_down_does_not_extend_the_device_deadline() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "slow_down"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let error = poll_device_token(
        &test_device_client().await,
        &format!("{}/token", server.uri()),
        "configured-client",
        /*oauth_resource*/ None,
        &details(),
        /*pkce*/ None,
    )
    .await
    .expect_err("slow_down must preserve the original expiry");
    assert_eq!(
        error.to_string(),
        "OAuth device code expired before authorization completed"
    );
}

#[tokio::test]
async fn device_login_cancellation_drops_the_in_flight_poll() {
    use axum::Router;
    use axum::routing::post;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tokio::sync::Notify;

    let admitted = Arc::new(Notify::new());
    let count = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
    let notify = admitted.clone();
    let requests = count.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/token",
                post(move || {
                    let notify = notify.clone();
                    let requests = requests.clone();
                    async move {
                        requests.fetch_add(1, Ordering::SeqCst);
                        notify.notify_one();
                        std::future::pending::<axum::response::Response>().await
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let polling = tokio::spawn(async move {
        poll_device_token(
            &test_device_client().await,
            &endpoint,
            "configured-client",
            /*oauth_resource*/ None,
            &details(),
            /*pkce*/ None,
        )
        .await
    });
    admitted.notified().await;
    polling.abort();
    assert!(polling.await.unwrap_err().is_cancelled());
    assert_eq!(count.load(Ordering::SeqCst), 1);
    server.abort();
}
