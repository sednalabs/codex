use super::*;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Verifier as _;
use std::io::Read as _;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

fn test_body(request_id: Uuid) -> Value {
    build_claim_body("w12345", 7, "2026-10-04T05:00:00Z", request_id).expect("fixed API claim body")
}

fn jwt(payload: Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("encode claims"));
    format!("{header}.{payload}.signature")
}

#[test]
fn api_bearer_consistency_is_bound_to_issuer_audience_subject_scope_and_expiry() {
    let principal = Uuid::from_u128(0xabcdef);
    let now = 1_800_000_000;
    let valid_claims = serde_json::json!({
        "iss": API_ISSUER,
        "aud": [API_AUDIENCE],
        "sub": principal.to_string(),
        "scope": "openid ops:write",
        "exp": now + 60,
    });
    let valid = validate_bearer(jwt(valid_claims.clone()), principal, now)
        .expect("the fixed profile consistency fields match");
    assert_eq!(valid.expires_at, now + 60);

    for (field, value) in [
        ("iss", serde_json::json!("https://wrong.example/")),
        ("aud", serde_json::json!("ops-mcp")),
        ("sub", serde_json::json!(Uuid::from_u128(2).to_string())),
        (
            "sub",
            serde_json::json!(principal.to_string().to_uppercase()),
        ),
        ("scope", serde_json::json!("ops:read")),
        ("exp", serde_json::json!(now)),
    ] {
        let mut claims = valid_claims.clone();
        claims[field] = value;
        assert!(
            validate_bearer(jwt(claims), principal, now).is_err(),
            "mismatched {field} must fail closed"
        );
    }
}

#[test]
fn api_precondition_and_proof_bind_the_exact_final_body_without_mcp_fields() {
    let request_id = Uuid::from_u128(5);
    let body = test_body(request_id);
    let operation = wire::canonical_api_operation(&body).expect("canonical API operation");
    assert_eq!(
        operation,
        format!(
            r#"{{"method":"POST","parameters":{{"claim_precondition":{{"expected_generation":7,"expected_updated_at":"2026-10-04T05:00:00Z","request_id":"{request_id}"}},"work_item_ref":"w12345"}},"route":"/v0/work-items/claim","transport":"http"}}"#
        )
        .into_bytes()
    );
    let changed = test_body(Uuid::from_u128(6));
    let changed_operation =
        wire::canonical_api_operation(&changed).expect("canonical changed API operation");
    assert_ne!(wire::digest(&operation), wire::digest(&changed_operation));
    assert_eq!(
        body["claim_precondition"]["request_id"],
        request_id.to_string()
    );

    let runtime_seed = [17_u8; 32];
    let runtime_incarnation = Uuid::from_u128(2);
    let execution_nonce = Uuid::from_u128(3);
    let proof = create_proof(
        "test-certificate",
        &runtime_seed,
        runtime_incarnation,
        execution_nonce,
        &body,
        100,
        140,
    )
    .expect("sign exact final API body");
    let mut segments = proof.split('.');
    let header: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(segments.next().expect("proof header"))
            .expect("decode proof header"),
    )
    .expect("parse proof header");
    let claims_bytes = URL_SAFE_NO_PAD
        .decode(segments.next().expect("proof claims"))
        .expect("decode proof claims");
    let signature = URL_SAFE_NO_PAD
        .decode(segments.next().expect("proof signature"))
        .expect("decode proof signature");
    assert!(segments.next().is_none());
    let claims: Value = serde_json::from_slice(&claims_bytes).expect("parse proof claims");
    assert_eq!(header["typ"], "runtime-invocation+jwt");
    assert_eq!(header["kid"], runtime_incarnation.to_string());
    assert_eq!(claims["recipient"], API_RECIPIENT);
    assert_eq!(claims["method"], "POST");
    assert_eq!(claims["operation_sha256"], wire::digest(&operation));
    assert!(claims.get("tool").is_none());
    assert!(claims.get("provider_thread_id").is_none());
    assert_eq!(claims["exp"], 140);

    let certificate_claims = wire::CertificateClaims {
        version: 1,
        runtime_public_key: "runtime-key".to_string(),
        runtime_incarnation,
        provider: API_PROVIDER.to_string(),
        artifact_sha256: "a".repeat(64),
        principal: Uuid::from_u128(1),
        project_id: 7,
        execution_lease_seconds: 40,
        work_item_ref: "w12345".to_string(),
        recipient: API_RECIPIENT.to_string(),
        mcp_server: None,
        method: "POST".to_string(),
        tool: None,
        purpose: "work-claim".to_string(),
        iat: 100,
        exp: 140,
    };
    let certificate_claims =
        serde_json::to_value(certificate_claims).expect("serialize API certificate claims");
    assert_eq!(certificate_claims["provider"], API_PROVIDER);
    assert!(certificate_claims.get("mcp_server").is_none());
    assert!(certificate_claims.get("tool").is_none());

    let public_key = SigningKey::from_bytes(&runtime_seed).verifying_key();
    public_key
        .verify(
            proof.rsplit_once('.').expect("signing input").0.as_bytes(),
            &ed25519_dalek::Signature::from_slice(&signature).expect("Ed25519 signature"),
        )
        .expect("runtime proof signature is valid");
}

#[test]
fn api_body_rejects_invalid_fixed_preconditions_before_network_access() {
    let request_id = Uuid::new_v4();
    assert!(build_claim_body("w012", 1, "2026-10-04T05:00:00Z", request_id).is_err());
    assert!(build_claim_body("w1", u64::MAX, "2026-10-04T05:00:00Z", request_id).is_err());
    assert!(build_claim_body("w1", 1, "not-a-timestamp", request_id).is_err());
}

#[test]
fn uncertain_api_post_is_not_replayed_and_diagnostics_do_not_expose_bearer() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback test endpoint");
    listener
        .set_nonblocking(true)
        .expect("make extra-connection check bounded");
    let address = listener.local_addr().expect("local address");
    let (request_tx, request_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let accept_deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < accept_deadline,
                        "fixed API client did not make its one request"
                    );
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept first API request: {error}"),
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("bound request read");
        let request = read_http_request(&mut stream).expect("read full API request");
        request_tx.send(request).expect("deliver captured request");
        drop(stream);

        let deadline = Instant::now() + Duration::from_millis(350);
        loop {
            match listener.accept() {
                Ok(_) => return true,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("check for a replayed API request: {error}"),
            }
        }
    });

    let client = api_http_client().expect("direct no-retry API client");
    let request_id = Uuid::from_u128(5);
    let body = test_body(request_id);
    let bearer = SecretString("synthetic-test-bearer-never-log".to_string());
    let result = send_claim_to(
        &client,
        &format!("http://{address}"),
        &bearer,
        request_id,
        "synthetic-proof",
        &body,
    );
    assert!(result.is_err());
    let error = format!(
        "{:#}",
        result.expect_err("closed response has unknown outcome")
    );
    assert!(error.contains("unknown; do not replay"));
    assert!(!error.contains(&bearer.0));

    let request = request_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("single protected request reached the test server");
    assert!(request.starts_with("POST /v0/work-items/claim HTTP/1.1\r\n"));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-test-bearer-never-log")
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("x-ops-runtime-proof: synthetic-proof")
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains(&format!("x-ops-request-id: {request_id}"))
    );
    assert!(request.contains(&body.to_string()));
    assert!(
        !server
            .join()
            .expect("server completes bounded replay check")
    );
}

fn read_http_request(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    let mut header_end = None;
    let mut content_length = 0;
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if header_end.is_none()
            && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = index + 4;
            let headers = String::from_utf8_lossy(&bytes[..end]);
            content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            header_end = Some(end);
        }
        if header_end.is_some_and(|end| bytes.len() >= end + content_length) {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}
