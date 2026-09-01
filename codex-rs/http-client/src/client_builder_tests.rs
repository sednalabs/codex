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
        .without_retries()
        .base_reqwest_builder()
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
        .build_with_custom_ca_fallback_using(ProxyRouting::Direct, |_| {
            Err(BuildCustomCaTransportError::InvalidCaFile {
                source_env: "TEST_CA_ENV",
                path: PathBuf::from("invalid-test-ca.pem"),
                detail: "synthetic invalid CA".to_string(),
            })
        });

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
