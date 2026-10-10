use super::*;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Verifier;
use serde::Deserialize;
use uuid::Uuid;
use zeroize::Zeroizing;

fn signer() -> RuntimeProofSigner {
    let seed = Zeroizing::new([17_u8; 32]);
    let key = SigningKey::from_bytes(&seed);
    let public_key = URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes());
    RuntimeProofSigner {
        certificate: "test-certificate".to_string(),
        claims: wire::CertificateClaims {
            version: 1,
            runtime_public_key: public_key,
            runtime_incarnation: Uuid::new_v4(),
            provider: "codex-native-runtime".to_string(),
            artifact_sha256: "a".repeat(64),
            principal: Uuid::new_v4(),
            project_id: 1,
            execution_lease_seconds: 900,
            work_item_ref: "example-work-item".to_string(),
            recipient: "https://ops.example/mcp".to_string(),
            mcp_server: "ops".to_string(),
            method: "tools/call".to_string(),
            tool: "work_item_claim".to_string(),
            purpose: "work-claim".to_string(),
            iat: unix_seconds().unwrap() - 5,
            exp: unix_seconds().unwrap() + 300,
        },
        seed,
    }
}

#[test]
fn certificate_validation_binds_key_artifact_and_fixed_execution_lease() {
    let signer = signer();
    let header = wire::CertificateHeader {
        alg: "EdDSA".to_string(),
        typ: "runtime-certificate+jwt".to_string(),
        kid: "issuer-key-1".to_string(),
    };
    let now = unix_seconds().unwrap();
    assert!(
        validate_certificate(
            &header,
            &signer.claims,
            &signer.seed,
            &signer.claims.artifact_sha256,
            now,
        )
        .is_ok()
    );

    let mut unbounded_lease = signer.claims.clone();
    unbounded_lease.execution_lease_seconds = 1801;
    assert!(
        validate_certificate(
            &header,
            &unbounded_lease,
            &signer.seed,
            &unbounded_lease.artifact_sha256,
            now,
        )
        .is_err()
    );

    let mut mismatched_key = signer.claims.clone();
    mismatched_key.runtime_public_key = "different-key".to_string();
    assert!(
        validate_certificate(
            &header,
            &mismatched_key,
            &signer.seed,
            &mismatched_key.artifact_sha256,
            now,
        )
        .is_err()
    );
    assert!(
        validate_certificate(
            &header,
            &signer.claims,
            &signer.seed,
            "different-artifact",
            now,
        )
        .is_err()
    );
}

#[test]
fn protection_drift_erases_the_process_signing_capability() {
    let mut state = SignerState::Ready(Box::new(signer()));
    assert!(
        erase_on_protection_failure(&mut state, Err(anyhow::anyhow!("protection changed")),)
            .is_err()
    );
    assert!(matches!(state, SignerState::Unconfigured));
}

fn parameters(note: &str) -> Value {
    serde_json::json!({
        "work_item_ref": "example-work-item",
        "note": note,
        "claim_precondition": {
            "expected_generation": 0,
            "expected_updated_at": "2026-10-04T05:00:00Z",
            "request_id": "8f14e45f-ea42-4d73-9ca9-9a8b35d1c05a",
        },
    })
}

#[derive(Deserialize)]
struct RequestHeaderOwned {
    alg: String,
    typ: String,
    kid: String,
}

fn decode_proof(proof: &str) -> (RequestHeaderOwned, wire::RequestClaims, Vec<u8>) {
    let mut parts = proof.split('.');
    let header = URL_SAFE_NO_PAD.decode(parts.next().unwrap()).unwrap();
    let claims = URL_SAFE_NO_PAD.decode(parts.next().unwrap()).unwrap();
    let signature = URL_SAFE_NO_PAD.decode(parts.next().unwrap()).unwrap();
    assert!(parts.next().is_none());
    (
        serde_json::from_slice(&header).unwrap(),
        serde_json::from_slice(&claims).unwrap(),
        signature,
    )
}

#[test]
fn signs_each_final_claim_request_with_native_context_and_fresh_nonce() {
    let signer = signer();
    let execution_nonce = Uuid::new_v4();
    let first = signer
        .create_claim_proof(&execution_nonce, "native-thread", &parameters("claim"))
        .unwrap()
        .unwrap();
    let second = signer
        .create_claim_proof(&Uuid::new_v4(), "delegate-thread", &parameters("claim"))
        .unwrap()
        .unwrap();
    let first_proof = first["proof"].as_str().unwrap();
    let second_proof = second["proof"].as_str().unwrap();
    let (header, claims, signature) = decode_proof(first_proof);
    let (_, delegate_claims, _) = decode_proof(second_proof);
    assert_eq!(header.alg, "EdDSA");
    assert_eq!(header.typ, "runtime-invocation+jwt");
    assert_eq!(header.kid, claims.runtime_incarnation.to_string());
    assert_eq!(claims.execution_nonce, execution_nonce);
    assert_eq!(claims.provider_thread_id, "native-thread");
    assert_ne!(claims.nonce, delegate_claims.nonce);
    assert_ne!(claims.execution_nonce, delegate_claims.execution_nonce);
    assert_eq!(claims.exp - claims.iat, 60);
    assert_eq!(first["certificate"], "test-certificate");

    let key_bytes: [u8; 32] = [17; 32];
    let verifying_key = SigningKey::from_bytes(&key_bytes).verifying_key();
    let signing_input = first_proof.rsplit_once('.').unwrap().0;
    verifying_key
        .verify(
            signing_input.as_bytes(),
            &ed25519_dalek::Signature::from_slice(&signature).unwrap(),
        )
        .unwrap();
    assert!(
        !first
            .to_string()
            .contains(&URL_SAFE_NO_PAD.encode(key_bytes))
    );
}

#[test]
fn refuses_foreign_scope_incomplete_precondition_and_expired_certificate() {
    let signer = signer();
    assert!(
        signer
            .create_claim_proof(
                &Uuid::new_v4(),
                "thread",
                &serde_json::json!({"work_item_ref":"w1"})
            )
            .is_err()
    );
    let mut missing = parameters("claim");
    missing
        .as_object_mut()
        .unwrap()
        .remove("claim_precondition");
    assert!(
        signer
            .create_claim_proof(&Uuid::new_v4(), "thread", &missing)
            .is_err()
    );
    let mut expired = signer;
    expired.claims.exp = unix_seconds().unwrap() - 1;
    assert!(
        expired
            .create_claim_proof(&Uuid::new_v4(), "thread", &parameters("claim"))
            .is_err()
    );
}

#[test]
fn selected_claim_rejects_a_mismatched_server_or_recipient() {
    let signer = signer();
    assert!(
        signer
            .matches_target("ops", Some("https://ops.example/mcp"), "work_item_claim",)
            .unwrap()
    );
    assert!(
        signer
            .matches_target(
                "ops",
                Some("https://foreign.example/mcp"),
                "work_item_claim",
            )
            .is_err()
    );
    assert!(
        signer
            .matches_target(
                "foreign",
                Some("https://ops.example/mcp"),
                "work_item_claim",
            )
            .is_err()
    );
    assert!(
        !signer
            .matches_target("unrelated", Some("https://other.example/mcp"), "some_tool")
            .unwrap()
    );
}

#[test]
fn changed_note_changes_signed_operation_digest() {
    let signer = signer();
    let first = signer
        .create_claim_proof(&Uuid::new_v4(), "thread", &parameters("one"))
        .unwrap()
        .unwrap();
    let second = signer
        .create_claim_proof(&Uuid::new_v4(), "thread", &parameters("two"))
        .unwrap()
        .unwrap();
    let (_, first_claims, _) = decode_proof(first["proof"].as_str().unwrap());
    let (_, second_claims, _) = decode_proof(second["proof"].as_str().unwrap());
    assert_ne!(
        first_claims.operation_sha256,
        second_claims.operation_sha256
    );
}

#[test]
fn issuer_certificate_and_native_invocation_use_one_positive_wire_vector() {
    let runtime_seed = Zeroizing::new([17_u8; 32]);
    let runtime_key = SigningKey::from_bytes(&runtime_seed);
    let runtime_incarnation = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
    let principal = Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap();
    let now = unix_seconds().unwrap();
    let claims = wire::CertificateClaims {
        version: 1,
        runtime_public_key: URL_SAFE_NO_PAD.encode(runtime_key.verifying_key().as_bytes()),
        runtime_incarnation,
        provider: "codex-native-runtime".to_string(),
        artifact_sha256: "a".repeat(64),
        principal,
        project_id: 7,
        execution_lease_seconds: 900,
        work_item_ref: "example-work-item".to_string(),
        recipient: "https://ops.example/mcp".to_string(),
        mcp_server: "ops".to_string(),
        method: "tools/call".to_string(),
        tool: "work_item_claim".to_string(),
        purpose: "work-claim".to_string(),
        iat: now,
        exp: now + 300,
    };
    let header = wire::CertificateHeader {
        alg: "EdDSA".to_string(),
        typ: "runtime-certificate+jwt".to_string(),
        kid: "test-issuer-key-1".to_string(),
    };
    let header_json = serde_json::json!({
        "alg": header.alg,
        "typ": header.typ,
        "kid": header.kid,
    });
    let claims_json = serde_json::json!({
        "version": claims.version,
        "runtime_public_key": claims.runtime_public_key,
        "runtime_incarnation": claims.runtime_incarnation,
        "provider": claims.provider,
        "artifact_sha256": claims.artifact_sha256,
        "principal": claims.principal,
        "project_id": claims.project_id,
        "execution_lease_seconds": claims.execution_lease_seconds,
        "work_item_ref": claims.work_item_ref,
        "recipient": claims.recipient,
        "mcp_server": claims.mcp_server,
        "method": claims.method,
        "tool": claims.tool,
        "purpose": claims.purpose,
        "iat": claims.iat,
        "exp": claims.exp,
    });
    let certificate_signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header_json).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims_json).unwrap()),
    );
    let issuer_key = SigningKey::from_bytes(&[23_u8; 32]);
    let certificate_signature = issuer_key.sign(certificate_signing_input.as_bytes());
    let certificate = format!(
        "{certificate_signing_input}.{}",
        URL_SAFE_NO_PAD.encode(certificate_signature.to_bytes()),
    );
    let (parsed_header, parsed_claims) = wire::parse_certificate(&certificate).unwrap();
    validate_certificate(
        &parsed_header,
        &parsed_claims,
        &runtime_seed,
        "a".repeat(64).as_str(),
        now,
    )
    .unwrap();
    assert_eq!(parsed_header.kid, "test-issuer-key-1");
    let issuer_verifying_key = issuer_key.verifying_key();
    issuer_verifying_key
        .verify(
            certificate_signing_input.as_bytes(),
            &ed25519_dalek::Signature::from_slice(
                &URL_SAFE_NO_PAD
                    .decode(certificate.rsplit_once('.').unwrap().1)
                    .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();

    let signer = RuntimeProofSigner {
        certificate: certificate.clone(),
        claims: parsed_claims,
        seed: runtime_seed,
    };
    let execution_nonce = Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap();
    let proof = signer
        .create_claim_proof(
            &execution_nonce,
            "codex-session-thread",
            &parameters("claim"),
        )
        .unwrap()
        .unwrap();
    let (request_header, request_claims, signature) =
        decode_proof(proof["proof"].as_str().unwrap());
    assert_eq!(request_header.alg, "EdDSA");
    assert_eq!(request_header.typ, "runtime-invocation+jwt");
    assert_eq!(request_header.kid, runtime_incarnation.to_string());
    assert_eq!(
        request_claims.certificate_sha256,
        wire::digest(certificate.as_bytes())
    );
    assert_eq!(request_claims.runtime_incarnation, runtime_incarnation);
    assert_eq!(request_claims.execution_nonce, execution_nonce);
    assert_eq!(request_claims.provider_thread_id, "codex-session-thread");
    assert_eq!(request_claims.recipient, "https://ops.example/mcp");
    assert_eq!(request_claims.method, "tools/call");
    assert_eq!(request_claims.tool, "work_item_claim");
    assert_eq!(request_claims.purpose, "work-claim");
    let operation = wire::canonical_operation("work_item_claim", &parameters("claim")).unwrap();
    assert_eq!(request_claims.operation_sha256, wire::digest(&operation));
    runtime_key
        .verifying_key()
        .verify(
            proof["proof"]
                .as_str()
                .unwrap()
                .rsplit_once('.')
                .unwrap()
                .0
                .as_bytes(),
            &ed25519_dalek::Signature::from_slice(&signature).unwrap(),
        )
        .unwrap();
}
