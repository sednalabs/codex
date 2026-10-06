#![cfg(target_os = "linux")]

#[path = "runtime_execution_proof/support.rs"]
mod runtime_execution_proof_support;

use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Verifier;
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use serde_json::Value;
use sha2::Digest;
use sha2::Sha256;
use std::fs;
use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use uuid::Uuid;

use runtime_execution_proof_support::ProtectedRuntimeFixture;

#[derive(Debug, Deserialize)]
struct CertificateClaims {
    version: u8,
    runtime_public_key: String,
    runtime_incarnation: Uuid,
    principal: Uuid,
    project_id: u64,
    execution_lease_seconds: u32,
    work_item_ref: String,
    recipient: String,
    mcp_server: String,
    method: String,
    tool: String,
    purpose: String,
    iat: i64,
    exp: i64,
    artifact_sha256: String,
    provider: String,
}

#[derive(Debug, Deserialize)]
struct InvocationClaims {
    version: u8,
    certificate_sha256: String,
    runtime_incarnation: Uuid,
    execution_nonce: Uuid,
    provider_thread_id: String,
    recipient: String,
    method: String,
    tool: String,
    purpose: String,
    operation_sha256: String,
    nonce: Uuid,
    iat: i64,
    exp: i64,
}

#[derive(Debug, Deserialize)]
struct JwsHeader {
    alg: String,
    typ: String,
    kid: String,
}

fn decode_compact_jws<T: for<'de> Deserialize<'de>>(
    value: &str,
) -> Result<(JwsHeader, T, Vec<u8>, &[u8])> {
    let mut parts = value.split('.');
    let header = URL_SAFE_NO_PAD.decode(parts.next().unwrap_or_default())?;
    let payload = URL_SAFE_NO_PAD.decode(parts.next().unwrap_or_default())?;
    let signature = URL_SAFE_NO_PAD.decode(parts.next().unwrap_or_default())?;
    anyhow::ensure!(parts.next().is_none(), "JWS has extra segments");
    let header = serde_json::from_slice(&header)?;
    let claims = serde_json::from_slice(&payload)?;
    let signing_input = value
        .rsplit_once('.')
        .map(|(input, _)| input)
        .unwrap_or_default();
    Ok((header, claims, signature, signing_input.as_bytes()))
}

fn assert_signature(key: &VerifyingKey, signature: &[u8], input: &[u8]) -> Result<()> {
    key.verify(input, &ed25519_dalek::Signature::from_slice(signature)?)?;
    Ok(())
}

fn sha256_hex(bytes: impl AsRef<[u8]>) -> String {
    let digest = Sha256::digest(bytes.as_ref());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn assert_no_secret(path: &Path, secrets: &[&str]) -> Result<()> {
    let file_type = fs::symlink_metadata(path)?.file_type();
    if file_type.is_file() {
        let bytes = fs::read(path)?;
        let text = String::from_utf8_lossy(&bytes);
        for secret in secrets {
            anyhow::ensure!(
                !text.contains(secret),
                "proof material leaked into {}",
                path.display()
            );
        }
    } else if file_type.is_dir() {
        for entry in fs::read_dir(path)? {
            assert_no_secret(&entry?.path(), secrets)?;
        }
    }
    Ok(())
}

fn assert_no_auth_files(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path)?.file_type().is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            anyhow::ensure!(
                !matches!(
                    name.as_ref(),
                    ".credentials.json" | "auth.json" | "access_token"
                ),
                "fixture created a credential file: {}",
                entry.path().display()
            );
            assert_no_auth_files(&entry.path())?;
        }
    }
    Ok(())
}

/// This deliberately requires an explicitly invoked disposable Linux root fixture.
/// It launches the actual CLI as an unprivileged child and uses only synthetic keys,
/// local mock endpoints and temporary state. It must never run on a live host.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly invoked disposable Linux root fixture"]
async fn protected_cli_root_and_delegate_calls_are_signed_and_redacted() -> Result<()> {
    eprintln!("runtime-proof-root-stage:started");
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "fixture must run as root");
    let fixture = ProtectedRuntimeFixture::start().await?;
    eprintln!("runtime-proof-root-stage:fixture_started");
    let result = fixture.run_cli().await?;

    eprintln!(
        "runtime-proof-root-stage:cli_{}",
        if result.status.success() {
            "succeeded"
        } else {
            "failed"
        }
    );
    anyhow::ensure!(result.status.success(), "codex exec exited unsuccessfully");
    let calls = fixture.claim_calls();
    anyhow::ensure!(
        calls.len() == 2,
        "expected root and delegate claim requests, got {}",
        calls.len()
    );
    eprintln!("runtime-proof-root-stage:claims_validated");
    anyhow::ensure!(
        !fixture.wait_targets().is_empty()
            && fixture
                .wait_targets()
                .iter()
                .all(|target| !target.is_empty()),
        "root did not wait on the spawned delegate before finalizing"
    );
    eprintln!("runtime-proof-root-stage:wait_targets_validated");
    anyhow::ensure!(
        fixture.accepted_wait_results() == vec![(true, false)],
        "root did not consume a non-timeout successful terminal result for the exact child"
    );
    eprintln!("runtime-proof-root-stage:wait_result_validated");
    let issuer_key = fixture.issuer_verifying_key();
    let mut invocation_nonces = Vec::new();
    let mut execution_nonces = Vec::new();
    let mut provider_thread_ids = Vec::new();
    let mut proofs = Vec::new();
    for call in &calls {
        let params = call
            .pointer("/params/arguments")
            .cloned()
            .unwrap_or(Value::Null);
        anyhow::ensure!(
            params.get("work_item_ref").and_then(Value::as_str) == Some(fixture.work_item_ref()),
            "claim target changed"
        );
        anyhow::ensure!(
            params
                .pointer("/claim_precondition/expected_generation")
                .and_then(Value::as_i64)
                == Some(3),
            "claim generation changed"
        );
        anyhow::ensure!(
            params
                .pointer("/claim_precondition/expected_updated_at")
                .and_then(Value::as_str)
                == Some("2026-10-04T05:00:00Z"),
            "claim revision changed"
        );
        anyhow::ensure!(
            params
                .pointer("/claim_precondition/request_id")
                .and_then(Value::as_str)
                == Some("8f14e45f-ea42-4d73-9ca9-9a8b35d1c05a"),
            "semantic request id changed"
        );
        let envelope = call
            .pointer("/params/_meta/runtime~1execution-proof")
            .cloned()
            .unwrap_or(Value::Null);
        let certificate = envelope
            .get("certificate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let invocation = envelope
            .get("proof")
            .and_then(Value::as_str)
            .unwrap_or_default();
        anyhow::ensure!(
            !certificate.is_empty() && !invocation.is_empty(),
            "protected call lacks proof envelope"
        );
        let (cert_header, certificate_claims, cert_signature, cert_input) =
            decode_compact_jws::<CertificateClaims>(certificate)?;
        assert_signature(&issuer_key, &cert_signature, cert_input)?;
        anyhow::ensure!(
            cert_header.alg == "EdDSA"
                && cert_header.typ == "runtime-certificate+jwt"
                && cert_header.kid == "test-issuer-key-1",
            "certificate JWS header mismatch"
        );
        anyhow::ensure!(
            certificate_claims.version == 1,
            "unsupported certificate version"
        );
        anyhow::ensure!(
            certificate_claims.provider == "codex-native-runtime",
            "certificate provider mismatch"
        );
        anyhow::ensure!(
            certificate_claims.project_id == 7
                && certificate_claims.principal
                    == Uuid::parse_str("22222222-2222-4222-8222-222222222222")?,
            "certificate principal scope mismatch"
        );
        anyhow::ensure!(
            certificate_claims.execution_lease_seconds == 900,
            "fixed execution lease mismatch"
        );
        anyhow::ensure!(
            certificate_claims.work_item_ref == fixture.work_item_ref(),
            "certificate work item mismatch"
        );
        anyhow::ensure!(
            certificate_claims.recipient == fixture.mcp_url(),
            "certificate recipient mismatch"
        );
        anyhow::ensure!(
            certificate_claims.mcp_server == "ops" && certificate_claims.method == "tools/call",
            "certificate MCP target mismatch"
        );
        anyhow::ensure!(
            certificate_claims.tool == "work_item_claim"
                && certificate_claims.purpose == "work-claim",
            "certificate operation mismatch"
        );
        anyhow::ensure!(
            certificate_claims.exp - certificate_claims.iat <= 1800,
            "certificate lifetime is unbounded"
        );
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
        anyhow::ensure!(
            certificate_claims.iat <= now && now < certificate_claims.exp,
            "certificate is outside its validity window"
        );
        anyhow::ensure!(
            certificate_claims.artifact_sha256 == fixture.artifact_sha256(),
            "certificate artifact mismatch"
        );
        let runtime_key_bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(&certificate_claims.runtime_public_key)?
            .try_into()
            .map_err(|_| anyhow::anyhow!("runtime public key has invalid size"))?;
        let runtime_key = VerifyingKey::from_bytes(&runtime_key_bytes)?;
        let (invocation_header, invocation_claims, proof_signature, proof_input) =
            decode_compact_jws::<InvocationClaims>(invocation)?;
        assert_signature(&runtime_key, &proof_signature, proof_input)?;
        anyhow::ensure!(
            invocation_header.alg == "EdDSA"
                && invocation_header.typ == "runtime-invocation+jwt"
                && invocation_header.kid == certificate_claims.runtime_incarnation.to_string(),
            "invocation JWS header mismatch"
        );
        anyhow::ensure!(
            invocation_claims.version == 1,
            "unsupported invocation version"
        );
        anyhow::ensure!(
            invocation_claims.certificate_sha256
                == format!("sha256:{}", sha256_hex(certificate.as_bytes())),
            "invocation certificate binding mismatch"
        );
        anyhow::ensure!(
            invocation_claims.runtime_incarnation == certificate_claims.runtime_incarnation,
            "invocation incarnation mismatch"
        );
        anyhow::ensure!(
            invocation_claims.recipient == fixture.mcp_url()
                && invocation_claims.method == "tools/call",
            "invocation transport mismatch"
        );
        anyhow::ensure!(
            invocation_claims.tool == "work_item_claim"
                && invocation_claims.purpose == "work-claim",
            "invocation operation mismatch"
        );
        anyhow::ensure!(
            invocation_claims.exp - invocation_claims.iat <= 60,
            "invocation lifetime is unbounded"
        );
        anyhow::ensure!(
            invocation_claims.iat <= now && now < invocation_claims.exp,
            "invocation is outside its validity window"
        );
        let operation = serde_json::json!({"transport":"mcp","method":"tools/call","tool":"work_item_claim","parameters":params});
        let mut canonical = Vec::new();
        serde_json_canonicalizer::to_writer(&operation, &mut canonical)?;
        anyhow::ensure!(
            invocation_claims.operation_sha256 == format!("sha256:{}", sha256_hex(canonical)),
            "invocation arguments digest mismatch"
        );
        invocation_nonces.push(invocation_claims.nonce);
        execution_nonces.push(invocation_claims.execution_nonce);
        provider_thread_ids.push(invocation_claims.provider_thread_id);
        proofs.push((certificate.to_string(), invocation.to_string()));
    }
    eprintln!("runtime-proof-root-stage:proofs_validated");
    anyhow::ensure!(
        invocation_nonces[0] != invocation_nonces[1],
        "per-call proof nonces were reused"
    );
    anyhow::ensure!(
        execution_nonces[0] != execution_nonces[1],
        "root and delegate execution contexts were reused"
    );
    anyhow::ensure!(
        !execution_nonces[0].is_nil() && !execution_nonces[1].is_nil(),
        "native execution nonce is missing"
    );
    anyhow::ensure!(
        !invocation_nonces[0].is_nil() && !invocation_nonces[1].is_nil(),
        "per-call nonce is missing"
    );
    anyhow::ensure!(
        !provider_thread_ids[0].is_empty() && !provider_thread_ids[1].is_empty(),
        "native thread evidence is absent"
    );
    anyhow::ensure!(
        provider_thread_ids[0] != provider_thread_ids[1],
        "root and delegate native thread evidence was reused"
    );
    anyhow::ensure!(
        calls[0]
            .pointer("/params/_meta/runtime~1execution-proof/proof")
            .is_some(),
        "root proof missing"
    );
    anyhow::ensure!(
        calls[1]
            .pointer("/params/_meta/runtime~1execution-proof/proof")
            .is_some(),
        "delegate proof missing"
    );

    let mut secrets = proofs
        .iter()
        .flat_map(|(certificate, proof)| [certificate.as_str(), proof.as_str()])
        .collect::<Vec<_>>();
    secrets.extend([
        fixture.provider_token(),
        runtime_execution_proof_support::OPS_BEARER_TOKEN,
    ]);
    let model_requests = fixture.model_requests();
    anyhow::ensure!(
        model_requests.len() >= 3,
        "expected follow-up model requests after echoed MCP results"
    );
    anyhow::ensure!(
        model_requests
            .iter()
            .any(|request| request_has_text(request, "root-claim-call")),
        "model did not receive root MCP result"
    );
    anyhow::ensure!(
        model_requests
            .iter()
            .any(|request| request_has_text(request, "delegate-claim-call")),
        "model did not receive delegate MCP result"
    );
    anyhow::ensure!(
        model_requests
            .iter()
            .any(|request| request_has_user_text(request, "delegate-claim")),
        "no model turn ran in the delegated session"
    );
    for request in model_requests {
        let serialized = request.to_string();
        for secret in &secrets {
            anyhow::ensure!(
                !serialized.contains(secret),
                "proof echoed to a later model request"
            );
        }
    }
    eprintln!("runtime-proof-root-stage:model_redaction_validated");
    let expected_model_auth = format!("Bearer {}", fixture.provider_token());
    let expected_mcp_auth = format!(
        "Bearer {}",
        runtime_execution_proof_support::OPS_BEARER_TOKEN
    );
    anyhow::ensure!(
        fixture
            .model_authorization_headers()
            .iter()
            .all(|header| header.as_deref() == Some(expected_model_auth.as_str())),
        "synthetic ChatGPT credential did not reach the model provider through production auth"
    );
    anyhow::ensure!(
        fixture
            .model_account_headers()
            .iter()
            .all(|header| header.as_deref()
                == Some(runtime_execution_proof_support::CHATGPT_ACCOUNT_ID)),
        "synthetic ChatGPT account id did not reach the provider through production auth"
    );
    anyhow::ensure!(
        fixture
            .mcp_authorization_headers()
            .iter()
            .all(|header| header.as_deref() == Some(expected_mcp_auth.as_str())),
        "synthetic Ops bearer did not reach the selected MCP transport"
    );
    eprintln!("runtime-proof-root-stage:auth_headers_validated");
    let emitted = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    for secret in &secrets {
        anyhow::ensure!(
            !emitted.contains(secret),
            "proof echoed to CLI events or output"
        );
    }
    eprintln!("runtime-proof-root-stage:cli_output_redacted");
    assert_no_secret(&fixture.fixture_root(), &secrets)?;
    assert_no_auth_files(&fixture.fixture_root())?;
    eprintln!("runtime-proof-root-stage:fixture_storage_clean");
    write_interoperability_vector_if_requested(&calls, &fixture.fixture_root())?;
    eprintln!("runtime-proof-root-stage:complete");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly invoked disposable Linux root fixture"]
async fn protected_trace_logging_does_not_echo_proof_or_bearer() -> Result<()> {
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "fixture must run as root");
    let fixture = ProtectedRuntimeFixture::start().await?;
    let result = fixture
        .run_cli_with_log_filter("root-claim", "trace")
        .await?;
    anyhow::ensure!(
        result.status.success(),
        "TRACE protected CLI failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let emitted = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    anyhow::ensure!(
        !emitted.contains(fixture.provider_token())
            && !emitted.contains(runtime_execution_proof_support::OPS_BEARER_TOKEN),
        "protected TRACE output exposed a credential"
    );
    let calls = fixture.claim_calls();
    anyhow::ensure!(
        calls.len() == 2,
        "TRACE control did not exercise root and delegate MCP calls"
    );
    for call in calls {
        let envelope = call
            .pointer("/params/_meta/runtime~1execution-proof")
            .cloned()
            .unwrap_or(Value::Null);
        for key in ["certificate", "proof"] {
            if let Some(secret) = envelope.get(key).and_then(Value::as_str) {
                anyhow::ensure!(
                    !emitted.contains(secret),
                    "protected TRACE output exposed proof"
                );
                anyhow::ensure!(
                    fixture
                        .model_requests()
                        .iter()
                        .all(|request| !request.to_string().contains(secret)),
                    "protected TRACE proof reached a later model request"
                );
            }
        }
    }
    for request in fixture.model_requests() {
        let serialized = request.to_string();
        anyhow::ensure!(
            !serialized.contains(fixture.provider_token())
                && !serialized.contains(runtime_execution_proof_support::OPS_BEARER_TOKEN),
            "protected TRACE credential reached a later model request"
        );
    }
    anyhow::ensure!(
        fixture.accepted_wait_results() == vec![(true, false)],
        "TRACE control did not consume the delegate's successful terminal result"
    );
    assert_no_secret(
        &fixture.fixture_root(),
        &[
            fixture.provider_token(),
            runtime_execution_proof_support::OPS_BEARER_TOKEN,
        ],
    )?;
    assert_no_auth_files(&fixture.fixture_root())?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly invoked disposable Linux root fixture"]
async fn protected_mcp_error_does_not_echo_proof_or_bearer() -> Result<()> {
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "fixture must run as root");
    let fixture = ProtectedRuntimeFixture::start_with_mcp_error().await?;
    let _ = fixture.run_cli().await?;
    let emitted = fixture.captured_output()?;
    anyhow::ensure!(
        !emitted.contains(fixture.provider_token())
            && !emitted.contains(runtime_execution_proof_support::OPS_BEARER_TOKEN),
        "protected MCP error exposed a credential"
    );
    let calls = fixture.claim_calls();
    anyhow::ensure!(
        calls.len() == 2,
        "error fixture did not exercise root and delegate"
    );
    for call in calls {
        let envelope = call
            .pointer("/params/_meta/runtime~1execution-proof")
            .cloned()
            .unwrap_or(Value::Null);
        for key in ["certificate", "proof"] {
            if let Some(secret) = envelope.get(key).and_then(Value::as_str) {
                anyhow::ensure!(
                    !emitted.contains(secret),
                    "protected MCP error exposed proof"
                );
                anyhow::ensure!(
                    fixture
                        .model_requests()
                        .iter()
                        .all(|request| !request.to_string().contains(secret)),
                    "protected MCP error proof reached a later model request"
                );
            }
        }
    }
    for request in fixture.model_requests() {
        let serialized = request.to_string();
        anyhow::ensure!(
            !serialized.contains(fixture.provider_token())
                && !serialized.contains(runtime_execution_proof_support::OPS_BEARER_TOKEN),
            "protected MCP error credential reached a later model request"
        );
    }
    assert_no_secret(
        &fixture.fixture_root(),
        &[
            fixture.provider_token(),
            runtime_execution_proof_support::OPS_BEARER_TOKEN,
        ],
    )?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly invoked disposable Linux root fixture"]
async fn invalid_auth_bootstraps_are_rejected_before_credential_egress() -> Result<()> {
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "fixture must run as root");
    for fault in [
        runtime_execution_proof_support::AuthFault::WrongContext,
        runtime_execution_proof_support::AuthFault::ConfigDigest,
        runtime_execution_proof_support::AuthFault::Expired,
        runtime_execution_proof_support::AuthFault::ProviderRecipient,
        runtime_execution_proof_support::AuthFault::CredentialClass,
        runtime_execution_proof_support::AuthFault::MutatedToken,
        runtime_execution_proof_support::AuthFault::ExtraTokenSegment,
    ] {
        let fixture = ProtectedRuntimeFixture::start().await?;
        let outcome = fixture.run_cli_with_auth_fault(Some(fault)).await;
        if matches!(
            fault,
            runtime_execution_proof_support::AuthFault::ProviderRecipient
        ) {
            let output = outcome?;
            anyhow::ensure!(
                !output.status.success(),
                "wrong effective provider endpoint was accepted"
            );
        } else {
            anyhow::ensure!(
                outcome.is_err(),
                "invalid protected auth bootstrap unexpectedly received ACK1"
            );
        }
        anyhow::ensure!(
            fixture.model_requests().is_empty(),
            "invalid auth reached provider egress"
        );
        anyhow::ensure!(
            fixture.model_authorization_headers().is_empty(),
            "invalid auth sent a provider Authorization header"
        );
        anyhow::ensure!(
            fixture.model_account_headers().is_empty(),
            "invalid auth sent a provider account header"
        );
        anyhow::ensure!(
            fixture.claim_calls().is_empty(),
            "invalid auth reached the protected MCP endpoint"
        );
        anyhow::ensure!(
            fixture.mcp_authorization_headers().is_empty(),
            "invalid auth sent an MCP bearer"
        );
        assert_no_secret(
            &fixture.fixture_root(),
            &[
                fixture.provider_token(),
                runtime_execution_proof_support::OPS_BEARER_TOKEN,
            ],
        )?;
        assert_no_auth_files(&fixture.fixture_root())?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly invoked disposable Linux root fixture"]
async fn delegate_provider_override_is_rejected_before_credential_egress() -> Result<()> {
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "fixture must run as root");
    let fixture = ProtectedRuntimeFixture::start().await?;
    let output = fixture.run_cli_with_prompt("role-provider-control").await?;
    anyhow::ensure!(
        output.status.success(),
        "root session did not complete after delegate rejection"
    );
    anyhow::ensure!(
        fixture
            .model_request_paths()
            .iter()
            .all(|path| path == "/v1/responses"),
        "delegate provider override reached its off-path endpoint"
    );
    anyhow::ensure!(
        fixture.claim_calls().len() == 1,
        "delegate MCP claim was not rejected before credential egress"
    );
    anyhow::ensure!(
        !fixture.wait_targets().is_empty()
            && fixture
                .wait_targets()
                .iter()
                .all(|target| !target.is_empty()),
        "root did not wait for the rejected delegate"
    );
    anyhow::ensure!(
        fixture.accepted_wait_results() == vec![(false, true)],
        "root did not consume the exact child's terminal provider-pin rejection"
    );
    let expected_authorization = format!("Bearer {}", fixture.provider_token());
    anyhow::ensure!(
        fixture
            .model_authorization_headers()
            .iter()
            .all(|header| header.as_deref() == Some(expected_authorization.as_str())),
        "root provider requests did not use the synthetic ephemeral credential"
    );
    assert_no_secret(
        &fixture.fixture_root(),
        &[
            fixture.provider_token(),
            runtime_execution_proof_support::OPS_BEARER_TOKEN,
        ],
    )?;
    assert_no_auth_files(&fixture.fixture_root())?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly invoked disposable Linux root fixture"]
async fn protected_mcp_redirect_is_stopped_before_recipient_change() -> Result<()> {
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "fixture must run as root");
    let fixture = ProtectedRuntimeFixture::start_with_redirect(/*redirect_claims*/ true).await?;
    let _ = fixture.run_cli().await?;
    anyhow::ensure!(
        fixture.claim_calls().len() == 2,
        "root and delegate must both reach the pinned MCP recipient"
    );
    anyhow::ensure!(
        fixture.accepted_wait_results() == vec![(true, false)],
        "root did not consume the spawned delegate's successful terminal result"
    );
    let expected_authorization = format!(
        "Bearer {}",
        runtime_execution_proof_support::OPS_BEARER_TOKEN
    );
    anyhow::ensure!(
        fixture
            .mcp_authorization_headers()
            .iter()
            .all(|header| header.as_deref() == Some(expected_authorization.as_str())),
        "protected calls did not use the selected MCP bearer"
    );
    anyhow::ensure!(
        fixture.followed_redirects() == 0,
        "protected HTTP transport followed a redirect to another recipient"
    );
    assert_no_secret(
        &fixture.fixture_root(),
        &[
            fixture.provider_token(),
            runtime_execution_proof_support::OPS_BEARER_TOKEN,
        ],
    )?;
    assert_no_auth_files(&fixture.fixture_root())?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly invoked disposable Linux root fixture"]
async fn delayed_mcp_response_is_redacted_after_protected_auth_expiry() -> Result<()> {
    anyhow::ensure!(unsafe { libc::geteuid() } == 0, "fixture must run as root");
    let fixture = ProtectedRuntimeFixture::start_delayed_response_expiry().await?;
    let _ = fixture
        .run_cli_with_auth_fault(Some(
            runtime_execution_proof_support::AuthFault::ExpireAfterFirstClaim,
        ))
        .await?;
    let mcp_requests = fixture.mcp_request_records();
    let claims = mcp_requests
        .iter()
        .filter(|request| request.http_method == "POST" && request.phase == "tools/call")
        .collect::<Vec<_>>();
    anyhow::ensure!(
        claims.len() == 1,
        "fixture did not issue exactly one claim call"
    );
    anyhow::ensure!(
        mcp_requests
            .iter()
            .any(|request| request.phase == "initialize")
            && mcp_requests
                .iter()
                .any(|request| request.phase == "tools/list"),
        "fixture did not record lifecycle request phases separately from the claim"
    );
    let expected_authorization = format!(
        "Bearer {}",
        runtime_execution_proof_support::OPS_BEARER_TOKEN
    );
    anyhow::ensure!(
        claims.iter().all(
            |request| request.authorization.as_deref() == Some(expected_authorization.as_str())
        ),
        "delayed claim did not use the selected MCP bearer"
    );
    let output = fixture.captured_output()?;
    if let Some(token) = fixture.expiring_provider_token() {
        anyhow::ensure!(
            !output.contains(&token),
            "expired provider token leaked to output"
        );
    }
    anyhow::ensure!(
        !output.contains(runtime_execution_proof_support::OPS_BEARER_TOKEN),
        "protected MCP bearer leaked to output"
    );
    assert_no_auth_files(&fixture.fixture_root())?;
    Ok(())
}

fn write_interoperability_vector_if_requested(calls: &[Value], fixture_root: &Path) -> Result<()> {
    let Some(directory) = std::env::var_os("CODEX_RUNTIME_PROOF_VECTOR_DIR") else {
        return Ok(());
    };
    let directory = Path::new(&directory);
    fs::create_dir_all(directory)?;
    let output_dir = fs::canonicalize(directory)?;
    let fixture_root = fs::canonicalize(fixture_root)?;
    anyhow::ensure!(
        !output_dir.starts_with(fixture_root),
        "interop vector output must be outside temporary runtime state"
    );
    let path = directory.join("synthetic-runtime-execution-proof-v1.json");
    let artifact = serde_json::json!({
        "format_version": 1,
        "provenance": "synthetic protected CLI integration fixture; issuer and runtime keys are test-only; runtime seed is omitted",
        "requests": calls,
    });
    fs::write(path, serde_json::to_vec_pretty(&artifact)?)?;
    Ok(())
}

fn request_has_text(request: &Value, expected: &str) -> bool {
    request.to_string().contains(expected)
}

fn request_has_user_text(request: &Value, expected: &str) -> bool {
    request
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("type").and_then(Value::as_str) == Some("message")
                    && item.get("role").and_then(Value::as_str) == Some("user")
                    && item
                        .get("content")
                        .and_then(Value::as_array)
                        .is_some_and(|content| {
                            content.iter().any(|part| {
                                part.get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|text| text.contains(expected))
                            })
                        })
            })
        })
}
