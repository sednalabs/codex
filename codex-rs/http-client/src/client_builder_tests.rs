use super::*;
use crate::HttpTransport;
use crate::Request;
use crate::RequestInitiation;
use crate::ReqwestTransport;
use crate::TransportError;
use http::HeaderValue;
use http::Method;
use http::header::AUTHORIZATION;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn retry_never_sends_one_credentialed_stream_after_http2_refusal() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("HTTP/2 listener should bind");
    let address = listener
        .local_addr()
        .expect("HTTP/2 listener should have an address");
    let credentialed_attempts = Arc::new(AtomicUsize::new(0));
    let server_attempts = Arc::clone(&credentialed_attempts);
    let server = tokio::spawn(async move {
        let (stream, _) = listener
            .accept()
            .await
            .expect("HTTP/2 listener should accept");
        let mut connection = h2::server::handshake(stream)
            .await
            .expect("HTTP/2 server handshake should complete");
        let (request, mut response) = connection
            .accept()
            .await
            .expect("client should open one stream")
            .expect("first HTTP/2 request should decode");
        if request.headers().get(AUTHORIZATION)
            == Some(&HeaderValue::from_static("Bearer wire-secret"))
        {
            server_attempts.fetch_add(1, Ordering::SeqCst);
        }
        response.send_reset(h2::Reason::REFUSED_STREAM);

        if let Ok(Some(Ok((request, mut response)))) =
            tokio::time::timeout(Duration::from_millis(250), connection.accept()).await
        {
            if request.headers().get(AUTHORIZATION)
                == Some(&HeaderValue::from_static("Bearer wire-secret"))
            {
                server_attempts.fetch_add(1, Ordering::SeqCst);
            }
            response.send_reset(h2::Reason::REFUSED_STREAM);
        }
    });
    let client = HttpClientBuilder::new()
        .without_redirects()
        .without_retries()
        .last_resort_reqwest_builder()
        .http2_prior_knowledge()
        .no_proxy()
        .build()
        .expect("HTTP/2 client should build");
    let transport = ReqwestTransport::from_http_client(HttpClient::new(client));
    let initiation = RequestInitiation::new(());
    let claim = initiation
        .claim()
        .expect("first attempt should claim authority");
    let mut request = Request::new(Method::POST, format!("http://{address}/responses"));
    request.headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static("Bearer wire-secret"),
    );

    let response = transport.execute(request);
    claim.acknowledge();
    assert!(matches!(response.await, Err(TransportError::Network(_))));
    server.await.expect("HTTP/2 server task should finish");
    assert_eq!(credentialed_attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn custom_ca_fallback_preserves_builder_configuration() {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("HTTP listener should bind");
    let address = listener
        .local_addr()
        .expect("HTTP listener should have an address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("HTTP listener should accept");
        let mut request = Vec::new();
        let mut chunk = [0_u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let bytes_read = stream.read(&mut chunk).expect("HTTP request should read");
            assert!(bytes_read > 0, "HTTP request should include headers");
            request.extend_from_slice(&chunk[..bytes_read]);
        }
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("HTTP listener should write response");
        String::from_utf8(request).expect("HTTP request should be UTF-8")
    });
    let mut headers = HeaderMap::new();
    headers.insert("x-builder-test", HeaderValue::from_static("preserved"));
    let client = HttpClientBuilder::new()
        .default_headers(headers)
        .build_with_custom_ca_fallback_using(
            ProxyRouting::Direct,
            |_| {
                Err(BuildCustomCaTransportError::InvalidCaFile {
                    source_env: "TEST_CA_ENV",
                    path: PathBuf::from("invalid-test-ca.pem"),
                    detail: "synthetic invalid CA".to_string(),
                })
            },
            reqwest::ClientBuilder::build,
        );

    let response = client
        .get(format!("http://{address}/fallback"))
        .send()
        .await
        .expect("fallback client should send request");
    assert!(response.status().is_success());
    let request = server.join().expect("HTTP listener should finish");
    assert!(
        request
            .lines()
            .any(|line| line.eq_ignore_ascii_case("x-builder-test: preserved"))
    );
}

fn forced_credential_bound_last_resort_client() -> HttpClient {
    HttpClientBuilder::new()
        .without_redirects()
        .without_retries()
        .build_with_custom_ca_fallback_using(
            ProxyRouting::Direct,
            |_| {
                Err(BuildCustomCaTransportError::InvalidCaFile {
                    source_env: "TEST_CA_ENV",
                    path: PathBuf::from("invalid-test-ca.pem"),
                    detail: "synthetic custom CA failure".to_string(),
                })
            },
            |_| Err::<reqwest::Client, _>("synthetic fallback builder failure"),
        )
}

#[tokio::test]
async fn credential_bound_double_build_fallback_never_follows_307_or_308() {
    for status in [307, 308] {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("redirect listener should bind");
        let address = listener
            .local_addr()
            .expect("redirect listener should have an address");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("first POST should connect");
            let first = read_http_headers(&mut stream).await;
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Redirect\r\nLocation: http://{address}/replayed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .expect("redirect response should write");
            let second_connection =
                tokio::time::timeout(Duration::from_millis(250), listener.accept())
                    .await
                    .is_ok();
            (first, second_connection)
        });
        let client = forced_credential_bound_last_resort_client();
        let response = client
            .post(format!("http://{address}/credentialed"))
            .header(AUTHORIZATION, "Bearer fallback-secret")
            .body("credentialed-body")
            .send()
            .await
            .expect("redirect response should be returned to the application");
        assert_eq!(response.status().as_u16(), status);
        let (first, second_connection) = server.await.expect("redirect server should finish");
        assert!(first.contains("authorization: Bearer fallback-secret"));
        assert!(!second_connection, "redirect must not create a second POST");
    }
}

async fn read_http_headers(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
        let bytes_read = stream
            .read(&mut chunk)
            .await
            .expect("HTTP request should read");
        assert!(bytes_read > 0, "HTTP request should include headers");
        request.extend_from_slice(&chunk[..bytes_read]);
    }
    String::from_utf8(request).expect("HTTP request should be UTF-8")
}

#[tokio::test]
async fn zero_write_stale_pool_recovery_sends_credentials_once() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("stale-pool listener should bind");
    let address = listener
        .local_addr()
        .expect("stale-pool listener should have an address");
    let server = tokio::spawn(async move {
        let (mut abandoned, _) = listener
            .accept()
            .await
            .expect("priming connection should be accepted");
        let priming = read_http_headers(&mut abandoned).await;
        abandoned
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("priming response should write");
        // Half-close the server write side after advertising keep-alive. Hyper-util may check out
        // this now-stale pooled connection, but its built-in recovery is limited to requests that
        // were canceled before any request bytes were written.
        abandoned
            .shutdown()
            .await
            .expect("stale connection should half-close");
        let mut abandoned_bytes = Vec::new();
        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            abandoned.read_to_end(&mut abandoned_bytes),
        )
        .await;

        let (mut fresh, _) = listener
            .accept()
            .await
            .expect("fresh connection should be accepted");
        let credentialed = read_http_headers(&mut fresh).await;
        fresh
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .expect("fresh response should write");
        (priming, abandoned_bytes, credentialed)
    });
    let client = HttpClientBuilder::new()
        .without_redirects()
        .without_retries()
        .base_reqwest_builder()
        .no_proxy()
        .build()
        .expect("credential-bound client should build");
    client
        .get(format!("http://{address}/prime"))
        .send()
        .await
        .expect("priming request should succeed");
    let response = client
        .post(format!("http://{address}/credentialed"))
        .header(AUTHORIZATION, "Bearer one-wire-secret")
        .body("credentialed-body")
        .send()
        .await
        .expect("zero-write stale-pool recovery should succeed");
    assert!(response.status().is_success());

    let (priming, abandoned_bytes, credentialed) =
        server.await.expect("stale-pool server should finish");
    assert!(!priming.contains("authorization:"));
    assert_eq!(abandoned_bytes, Vec::<u8>::new());
    assert!(credentialed.contains("authorization: Bearer one-wire-secret"));
}
