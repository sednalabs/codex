use super::*;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Verifier as _;
use std::io::Read as _;
use std::io::Write as _;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

fn test_body(request_id: Uuid) -> Value {
    build_claim_body(
        "w12345",
        /*expected_generation*/ 7,
        "2026-10-04T05:00:00Z",
        request_id,
    )
    .expect("fixed API claim body")
}

fn jwt(payload: Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).expect("encode claims"));
    format!("{header}.{payload}.signature")
}

fn signed_api_certificate(runtime_seed: &[u8; 32], runtime_incarnation: Uuid) -> String {
    let claims = wire::CertificateClaims {
        version: 1,
        runtime_public_key: URL_SAFE_NO_PAD.encode(
            SigningKey::from_bytes(runtime_seed)
                .verifying_key()
                .as_bytes(),
        ),
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
    let header = wire::CertificateHeader {
        alg: "EdDSA".to_string(),
        typ: "runtime-certificate+jwt".to_string(),
        kid: "test-issuer".to_string(),
    };
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).expect("encode certificate header")),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("encode certificate claims")),
    );
    let issuer = SigningKey::from_bytes(&[23_u8; 32]);
    let signature = issuer.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

#[test]
fn api_bearer_consistency_is_bound_to_issuer_audience_subject_scope_and_expiry() {
    let principal = Uuid::from_u128(0xabcdef);
    let now = 1_800_000_000;
    let valid_claims = serde_json::json!({
        "iss": API_ISSUER,
        "aud": API_AUDIENCE,
        "sub": principal.to_string(),
        "scope": API_SCOPE,
        "exp": now + 60,
    });
    let valid = validate_bearer(jwt(valid_claims.clone()), principal, now)
        .expect("the fixed profile consistency fields match");
    assert_eq!(valid.expires_at, now + 60);

    let shared_ops_audiences = serde_json::json!({
        "iss": API_ISSUER,
        "aud": [
            API_AUDIENCE,
            "ops-mcp-service",
            "ops-mcp",
            "https://ops-mcp.grantmatler.com/mcp",
        ],
        "sub": principal.to_string(),
        "scope": API_SCOPE,
        "exp": now + 60,
    });
    assert!(
        validate_bearer(jwt(shared_ops_audiences), principal, now).is_ok(),
        "the fixed API audience may be a member of the existing Ops audience set"
    );

    for (field, value) in [
        ("iss", serde_json::json!("https://wrong.example/")),
        ("aud", serde_json::json!("ops-mcp")),
        ("aud", serde_json::json!(["ops-mcp", "account"])),
        ("sub", serde_json::json!(Uuid::from_u128(2).to_string())),
        (
            "sub",
            serde_json::json!(principal.to_string().to_uppercase()),
        ),
        ("scope", serde_json::json!("ops:read")),
        ("scope", serde_json::json!("openid ops:write")),
        ("scp", serde_json::json!([API_SCOPE, "ops:read"])),
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
fn api_bearer_provisioning_validates_profile_without_activating_a_claim_policy() {
    let principal = Uuid::from_u128(0xabcdef);
    let other_principal = Uuid::from_u128(0x123456);
    let now = 1_800_000_000;
    let token = jwt(serde_json::json!({
        "iss": API_ISSUER,
        "aud": API_AUDIENCE,
        "sub": principal.to_string(),
        "scope": API_SCOPE,
        "exp": now + 60,
    }));

    let provisioned = validate_bearer_for_provisioning(token.clone(), now)
        .expect("fixed OAuth profile is checked before volatile storage");
    assert_eq!(provisioned.expires_at, now + 60);
    assert!(validate_bearer(token.clone(), other_principal, now).is_err());
    assert!(validate_bearer(token, principal, now).is_ok());
}

#[test]
fn api_bearer_credential_envelope_has_a_fresh_cleanup_identity() {
    let token = SecretString("synthetic-api-token".to_string());
    let first = encode_bearer_credential(&token).expect("encode first credential");
    let second = encode_bearer_credential(&token).expect("encode replacement credential");
    assert_ne!(first.as_slice(), second.as_slice());
    assert_eq!(
        decode_bearer_credential(&first)
            .expect("decode loaded credential")
            .0,
        token.0
    );
    let mut invalid: Value = serde_json::from_slice(&first).expect("parse envelope");
    invalid["generation"] = serde_json::json!(Uuid::nil().to_string());
    assert!(
        decode_bearer_credential(&serde_json::to_vec(&invalid).expect("encode invalid")).is_err()
    );
}

#[test]
fn api_bearer_rejects_inconsistent_scope_claims() {
    let principal = Uuid::from_u128(0xabcdef);
    let now = 1_800_000_000;
    let claims = serde_json::json!({
        "iss": API_ISSUER,
        "aud": API_AUDIENCE,
        "sub": principal.to_string(),
        "scope": API_SCOPE,
        "scp": "ops:read",
        "exp": now + 60,
    });
    assert!(validate_bearer(jwt(claims), principal, now).is_err());
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
    let certificate = signed_api_certificate(&runtime_seed, runtime_incarnation);
    let proof = create_proof(
        &certificate,
        &runtime_seed,
        runtime_incarnation,
        execution_nonce,
        &body,
        /*now*/ 100,
        /*certificate_expires_at*/ 140,
    )
    .expect("sign exact final API body");
    let envelope = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&wire::ProofEnvelope {
            certificate: &certificate,
            proof: &proof,
        })
        .expect("serialize API proof envelope"),
    );
    let envelope: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(envelope)
            .expect("decode API proof envelope"),
    )
    .expect("parse API proof envelope");
    assert_eq!(
        envelope,
        serde_json::json!({
            "certificate": certificate,
            "proof": proof,
        })
    );
    let proof = envelope["proof"].as_str().expect("envelope proof");
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
    assert!(
        build_claim_body(
            "w012",
            /*expected_generation*/ 1,
            "2026-10-04T05:00:00Z",
            request_id
        )
        .is_err()
    );
    assert!(build_claim_body("w1", u64::MAX, "2026-10-04T05:00:00Z", request_id).is_err());
    assert!(
        build_claim_body(
            "w1",
            /*expected_generation*/ 1,
            "not-a-timestamp",
            request_id
        )
        .is_err()
    );
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
        let accept_request = |description: &str| {
            let accept_deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => return stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < accept_deadline, "{description}");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept API request: {error}"),
                }
            }
        };
        let mut stream = accept_request("fixed API client did not make its claim request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("bound request read");
        let claim_request = read_http_request(&mut stream).expect("read full API claim request");
        request_tx
            .send(claim_request)
            .expect("deliver captured claim request");
        drop(stream);

        let mut receipt_stream = accept_request("fixed API client did not read execution receipt");
        receipt_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("bound receipt read");
        let receipt_request =
            read_http_request(&mut receipt_stream).expect("read execution receipt request");
        request_tx
            .send(receipt_request)
            .expect("deliver captured receipt request");
        let request_id = Uuid::from_u128(5);
        let receipt_body = serde_json::json!({
            "work_item_ref": "w12345",
            "request_id": request_id,
            "receipt": {
                "historical": null,
                "current_claim": {"work_item_ref": "w12345"},
            },
        })
        .to_string();
        write!(
            receipt_stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            receipt_body.len(),
            receipt_body
        )
        .expect("return unresolved execution receipt");
        drop(receipt_stream);

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
    let runtime_seed = [17_u8; 32];
    let runtime_incarnation = Uuid::from_u128(2);
    let certificate = signed_api_certificate(&runtime_seed, runtime_incarnation);
    let proof = create_proof(
        &certificate,
        &runtime_seed,
        runtime_incarnation,
        Uuid::from_u128(3),
        &body,
        /*now*/ 100,
        /*certificate_expires_at*/ 140,
    )
    .expect("sign actual final API body");
    let result = send_claim_to(
        &client,
        &format!("http://{address}"),
        &bearer,
        request_id,
        &certificate,
        &proof,
        &body,
    );
    assert!(result.is_err());
    let error = format!(
        "{:#}",
        result.expect_err("closed response has unknown outcome")
    );
    assert!(error.contains("unresolved; do not replay"));
    assert!(!error.contains(&bearer.0));

    let claim_request = request_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("one protected claim request reached the test server");
    let receipt_request = request_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("one execution receipt request reached the test server");
    assert!(claim_request.starts_with("POST /v0/work-items/claim HTTP/1.1\r\n"));
    assert!(
        claim_request
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-test-bearer-never-log")
    );
    let proof_header = claim_request
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("x-ops-runtime-proof")
                .then_some(value.trim())
        })
        .expect("captured proof header");
    let proof_envelope: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(proof_header)
            .expect("decode base64url proof envelope"),
    )
    .expect("parse proof envelope expected by Ops API/DAS");
    assert_eq!(
        proof_envelope,
        serde_json::json!({
            "certificate": certificate,
            "proof": proof,
        })
    );
    let invocation = proof_envelope["proof"].as_str().expect("enveloped proof");
    let mut segments = invocation.split('.');
    let _header = segments.next().expect("invocation header");
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(segments.next().expect("invocation claims"))
            .expect("decode invocation claims"),
    )
    .expect("parse invocation claims");
    assert_eq!(
        claims["operation_sha256"],
        wire::digest(&wire::canonical_api_operation(&body).expect("canonical operation"))
    );
    assert!(segments.next().is_some(), "invocation signature is present");
    assert!(segments.next().is_none());
    assert!(
        claim_request
            .to_ascii_lowercase()
            .contains(&format!("x-ops-request-id: {request_id}"))
    );
    assert!(claim_request.contains(&body.to_string()));
    assert!(receipt_request.starts_with("POST /v0/work-items/execution-receipt/read HTTP/1.1\r\n"));
    assert!(
        receipt_request
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-test-bearer-never-log")
    );
    assert!(
        !receipt_request
            .to_ascii_lowercase()
            .contains("x-ops-runtime-proof:")
    );
    let receipt_body = receipt_request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("execution receipt request body");
    let receipt_body: Value = serde_json::from_str(receipt_body).expect("parse receipt body");
    assert_eq!(
        receipt_body,
        serde_json::json!({
            "work_item_ref": "w12345",
            "request_id": request_id,
        })
    );
    assert!(
        !server
            .join()
            .expect("server completes bounded no-replay check")
    );
}

#[test]
fn uncertain_api_post_recovers_only_from_matching_historical_receipt() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback test endpoint");
    let listener = listener;
    listener
        .set_nonblocking(true)
        .expect("make extra-connection check bounded");
    let address = listener.local_addr().expect("local address");
    let request_id = Uuid::from_u128(5);
    let (request_tx, request_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        let accept_request = |description: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => return stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "{description}");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept API request: {error}"),
                }
            }
        };

        let mut claim_stream = accept_request("claim request was not sent");
        claim_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("bound claim request read");
        request_tx
            .send(read_http_request(&mut claim_stream).expect("read claim request"))
            .expect("capture claim request");
        drop(claim_stream);

        let mut receipt_stream = accept_request("receipt read was not sent");
        receipt_stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("bound receipt request read");
        request_tx
            .send(read_http_request(&mut receipt_stream).expect("read receipt request"))
            .expect("capture receipt request");
        let receipt_body = serde_json::json!({
            "work_item_ref": "w12345",
            "request_id": request_id,
            "receipt": {
                "historical": {
                    "historical": true,
                    "outcome": {
                        "historical": false,
                        "work_item": {"work_item_ref": "w12345"},
                        "execution_claim": {"execution_instance_id": "claim-instance"},
                    },
                },
                "current_claim": null,
            },
        })
        .to_string();
        write!(
            receipt_stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            receipt_body.len(),
            receipt_body
        )
        .expect("return matching historical receipt");
        drop(receipt_stream);

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
                Err(error) => panic!("check for replayed claim: {error}"),
            }
        }
    });

    let client = api_http_client().expect("direct no-retry API client");
    let body = test_body(request_id);
    let runtime_seed = [17_u8; 32];
    let runtime_incarnation = Uuid::from_u128(2);
    let certificate = signed_api_certificate(&runtime_seed, runtime_incarnation);
    let proof = create_proof(
        &certificate,
        &runtime_seed,
        runtime_incarnation,
        Uuid::from_u128(3),
        &body,
        /*now*/ 100,
        /*certificate_expires_at*/ 140,
    )
    .expect("sign exact API claim");
    send_claim_to(
        &client,
        &format!("http://{address}"),
        &SecretString("synthetic-test-bearer".to_string()),
        request_id,
        &certificate,
        &proof,
        &body,
    )
    .expect("matching stored history settles the unknown POST");

    let claim_request = request_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("captured single claim request");
    let receipt_request = request_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("captured single receipt read");
    assert!(claim_request.starts_with("POST /v0/work-items/claim HTTP/1.1\r\n"));
    assert!(receipt_request.starts_with("POST /v0/work-items/execution-receipt/read HTTP/1.1\r\n"));
    assert!(
        receipt_request
            .to_ascii_lowercase()
            .contains("authorization: bearer synthetic-test-bearer")
    );
    assert!(
        !receipt_request
            .to_ascii_lowercase()
            .contains("x-ops-runtime-proof:")
    );
    let receipt_body = receipt_request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("execution receipt request body");
    let receipt_body: Value = serde_json::from_str(receipt_body).expect("parse receipt body");
    assert_eq!(
        receipt_body,
        serde_json::json!({
            "work_item_ref": "w12345",
            "request_id": request_id,
        })
    );
    assert!(
        !server
            .join()
            .expect("server completes bounded no-replay check")
    );
}

#[test]
fn execution_receipt_requires_matching_historical_claim_shape() {
    let request_id = Uuid::from_u128(5);
    let body = serde_json::json!({
        "work_item_ref": "w12345",
        "request_id": request_id,
        "receipt": {
            "historical": null,
            "current_claim": {"work_item_ref": "w12345"},
        },
    })
    .to_string();
    assert_eq!(
        parse_execution_receipt(body.as_bytes(), "w12345", request_id)
            .expect("parse scoped unresolved receipt"),
        ExecutionReceiptResolution::Unresolved
    );
    assert!(parse_execution_receipt(body.as_bytes(), "w12346", request_id).is_err());
    assert!(parse_execution_receipt(body.as_bytes(), "w12345", Uuid::from_u128(6)).is_err());

    let malformed_history = serde_json::json!({
        "work_item_ref": "w12345",
        "request_id": request_id,
        "receipt": {
            "historical": {"historical": true, "outcome": {}},
            "current_claim": null,
        },
    })
    .to_string();
    assert!(parse_execution_receipt(malformed_history.as_bytes(), "w12345", request_id).is_err());

    let mismatched_history = serde_json::json!({
        "work_item_ref": "w12345",
        "request_id": request_id,
        "receipt": {
            "historical": {
                "historical": true,
                "outcome": {
                    "historical": false,
                    "work_item": {"work_item_ref": "w12346"},
                    "execution_claim": {},
                },
            },
            "current_claim": null,
        },
    })
    .to_string();
    assert!(parse_execution_receipt(mismatched_history.as_bytes(), "w12345", request_id).is_err());
}

#[test]
fn api_200_claim_response_requires_the_matching_claimed_target() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback test endpoint");
    let address = listener.local_addr().expect("local address");
    let response_body = serde_json::json!({
        "outcome": "claimed",
        "work_item": {"work_item_ref": "w12345", "status": "in_progress"},
    })
    .to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept one API request");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("bound request read");
        let _request = read_http_request(&mut stream).expect("read full API request");
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        )
        .expect("write fixed claim receipt");
    });

    let client = api_http_client().expect("direct no-retry API client");
    let request_id = Uuid::from_u128(5);
    let body = test_body(request_id);
    let runtime_seed = [17_u8; 32];
    let runtime_incarnation = Uuid::from_u128(2);
    let certificate = signed_api_certificate(&runtime_seed, runtime_incarnation);
    let proof = create_proof(
        &certificate,
        &runtime_seed,
        runtime_incarnation,
        Uuid::from_u128(3),
        &body,
        /*now*/ 100,
        /*certificate_expires_at*/ 140,
    )
    .expect("sign final API body");
    send_claim_to(
        &client,
        &format!("http://{address}"),
        &SecretString("synthetic-test-bearer".to_string()),
        request_id,
        &certificate,
        &proof,
        &body,
    )
    .expect("matching claim receipt confirms success");
    server.join().expect("bounded receipt server completes");

    for (receipt, expected_error) in [
        (
            br#"{"outcome":"rejected"}"#.as_slice(),
            "no matching claimed receipt",
        ),
        (
            br#"{"outcome":"claimed","work_item":{"work_item_ref":"w99999"}}"#.as_slice(),
            "no matching claimed receipt",
        ),
        (br#"not-json"#.as_slice(), "outcome unknown; do not replay"),
        (
            br#"{"outcome":"claimed","work_item":{"work_item_ref":"w12345"},"extra":true}"#
                .as_slice(),
            "outcome unknown; do not replay",
        ),
    ] {
        let error = validate_claim_receipt(receipt, "w12345")
            .expect_err("a non-matching or malformed receipt cannot report success");
        assert!(format!("{error:#}").contains(expected_error));
        assert!(!format!("{error:#}").contains("not-json"));
    }
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
