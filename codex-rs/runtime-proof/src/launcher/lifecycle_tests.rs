use super::*;
use ed25519_dalek::Signature;
use ed25519_dalek::Verifier as _;
use serde_json::Value;
use std::io::BufRead as _;
use std::net::TcpListener;
use std::time::Instant;

fn start_event() -> Event {
    Event {
        version: 1,
        principal_id: Uuid::from_u128(1),
        project_id: 17,
        work_item_ref: "w12345".to_string(),
        issuer_kid: "issuer-v1".to_string(),
        certificate_sha256: wire::certificate_digest(b"synthetic-certificate"),
        runtime_incarnation: Uuid::from_u128(2),
        event_id: Uuid::from_u128(3),
        phase: Phase::Start,
        source: Source::Launcher,
        manager_invocation: Uuid::from_u128(4),
        observer_artifact_sha256: "a".repeat(64),
        observed_at: timestamp(1_791_158_405).expect("timestamp"),
        cause: Cause::LaunchRegistered,
        exit_code: None,
        signal: None,
        absence_proven: false,
    }
}

// Explicitly ignored during ordinary test runs. This is a synthetic interop
// generator, not an issuer or a production credential interface.
#[test]
#[ignore]
fn synthetic_interop_packet() {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        now: i64,
        principal_id: Uuid,
        project_id: u64,
        work_item_ref: String,
        issuer_kid: String,
        runtime_incarnation: Uuid,
        start_event_id: Uuid,
        end_event_id: Uuid,
        manager_invocation: Uuid,
        artifact_sha256: String,
        observer_artifact_sha256: String,
        mcp_recipient: String,
        lifecycle_recipient: String,
    }
    let input: Input = serde_json::from_str(
        &std::env::var("CODEX_LIFECYCLE_VECTOR_INPUT").expect("synthetic vector JSON"),
    )
    .expect("valid synthetic vector input");
    let issuer_seed = [23_u8; 32];
    let runtime_seed = [7_u8; 32];
    let issuer_public = URL_SAFE_NO_PAD.encode(
        SigningKey::from_bytes(&issuer_seed).verifying_key().as_bytes(),
    );
    let policy = super::super::LauncherPolicy {
        version: 1,
        principal: input.principal_id,
        project_id: input.project_id,
        work_item_ref: input.work_item_ref.clone(),
        issuer_kid: input.issuer_kid.clone(),
        issuer_public_key: issuer_public.clone(),
        execution_lease_seconds: 300,
        target: super::super::LauncherTarget::Http(super::super::ApiClaimConfig {
            launcher_sha256: input.observer_artifact_sha256.clone(),
            expected_generation: 0,
            expected_updated_at: String::new(),
        }),
    };
    let claims = wire::CertificateClaims {
        version: 2,
        runtime_public_key: URL_SAFE_NO_PAD.encode(
            SigningKey::from_bytes(&runtime_seed).verifying_key().as_bytes(),
        ),
        runtime_incarnation: input.runtime_incarnation,
        provider: "codex-native-runtime".to_string(),
        artifact_sha256: input.artifact_sha256,
        principal: input.principal_id,
        project_id: input.project_id,
        execution_lease_seconds: 300,
        work_item_ref: input.work_item_ref.clone(),
        recipient: input.mcp_recipient,
        mcp_server: Some(super::super::MCP_SERVER.to_string()),
        method: "tools/call".to_string(),
        tool: None,
        purpose: None,
        operations: Some(super::super::MCP_MUTATION_TOOLS.iter().zip(["work-claim", "work-release"]).map(|(tool, purpose)| wire::ProtectedOperation {
            tool: (*tool).to_string(),
            purpose: purpose.to_string(),
        }).collect()),
        iat: input.now,
        exp: input.now.checked_add(300).expect("timestamp overflow"),
    };
    let certificate = super::super::issue_certificate(&claims, &issuer_seed, &policy)
        .expect("issue synthetic native-v2 certificate");
    let mut event = Event {
        version: 1,
        principal_id: input.principal_id,
        project_id: input.project_id,
        work_item_ref: input.work_item_ref,
        issuer_kid: input.issuer_kid,
        certificate_sha256: wire::certificate_digest(certificate.as_bytes()),
        runtime_incarnation: input.runtime_incarnation,
        event_id: input.start_event_id,
        phase: Phase::Start,
        source: Source::Launcher,
        manager_invocation: input.manager_invocation,
        observer_artifact_sha256: input.observer_artifact_sha256,
        observed_at: timestamp(input.now).expect("timestamp"),
        cause: Cause::LaunchRegistered,
        exit_code: None,
        signal: None,
        absence_proven: false,
    };
    let packet = |event: Event| {
        let pending = PendingEvent::new(event, input.now).expect("pending lifecycle event");
        let body = serde_json::to_value(&pending.event).expect("event body");
        let canonical = wire::canonical_lifecycle_operation(&body).expect("canonical operation");
        let header = pending.header(&certificate, &issuer_seed, &input.lifecycle_recipient)
            .expect("lifecycle header");
        serde_json::json!({
            "body": body,
            "canonical_operation_utf8": String::from_utf8(canonical).expect("canonical utf8"),
            "operation_sha256": pending.operation_sha256,
            "x_ops_runtime_lifecycle": header.as_str(),
        })
    };
    let start = packet(event.clone());
    event.event_id = input.end_event_id;
    event.phase = Phase::End;
    event.cause = Cause::Exited;
    event.exit_code = Some(0);
    event.absence_proven = true;
    let end = packet(event);
    println!("SYNTHETIC_LIFECYCLE_PACKET={}", serde_json::json!({
        "issuer_public_key_base64url": issuer_public,
        "certificate": certificate,
        "start": start,
        "end": end,
    }));
}

#[test]
fn lifecycle_witness_binds_final_operation_and_issuer() {
    let seed = [7_u8; 32];
    let pending = PendingEvent::new(start_event(), 1_791_158_405).expect("pending event");
    let header = pending
        .header("synthetic-certificate", &seed, "https://example.invalid")
        .expect("lifecycle envelope");
    let envelope: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(header.as_str())
            .expect("header base64"),
    )
    .expect("envelope JSON");
    let witness = envelope["witness"].as_str().expect("witness");
    let parts = witness.split('.').collect::<Vec<_>>();
    assert_eq!(parts.len(), 3);
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).expect("claims base64"))
            .expect("claims JSON");
    assert_eq!(
        claims["operation_sha256"].as_str(),
        Some(pending.operation_sha256.as_str())
    );
    assert_eq!(
        claims["certificate_sha256"].as_str(),
        Some(pending.event.certificate_sha256.as_str())
    );
    assert_eq!(claims["route"], ROUTE);
    let signature =
        Signature::from_slice(&URL_SAFE_NO_PAD.decode(parts[2]).expect("signature base64"))
            .expect("signature");
    SigningKey::from_bytes(&seed)
        .verifying_key()
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .expect("issuer signature");
    assert!(
        pending
            .header("different-certificate", &seed, "https://example.invalid")
            .is_err()
    );
    let mut changed = pending;
    changed.event.work_item_ref = "w54321".to_string();
    assert!(
        changed
            .header("synthetic-certificate", &seed, "https://example.invalid")
            .is_err()
    );
}

#[test]
fn end_event_requires_actual_closed_cause_and_absence() {
    let mut event = start_event();
    event.phase = Phase::End;
    event.cause = Cause::Exited;
    event.exit_code = Some(0);
    assert!(PendingEvent::new(event.clone(), 1_791_158_405).is_err());
    event.absence_proven = true;
    assert!(PendingEvent::new(event.clone(), 1_791_158_405).is_ok());
    event.signal = Some(9);
    assert!(PendingEvent::new(event, 1_791_158_405).is_err());
}

#[test]
fn receipt_requires_exact_event_and_proof_interval() {
    let pending = PendingEvent::new(start_event(), 1_791_158_405).expect("pending event");
    let event = &pending.event;
    let mut receipt = Receipt {
        event_id: event.event_id.to_string(),
        issuer_kid: event.issuer_kid.clone(),
        certificate_sha256: event.certificate_sha256.clone(),
        runtime_incarnation: event.runtime_incarnation.to_string(),
        work_item_id: 12345,
        project_id: event.project_id,
        principal_id: event.principal_id.to_string(),
        phase: event.phase,
        manager_invocation: event.manager_invocation.to_string(),
        observer_artifact_sha256: event.observer_artifact_sha256.clone(),
        operation_sha256: pending.operation_sha256.clone(),
        source: event.source,
        cause: event.cause,
        exit_code: None,
        signal: None,
        absence_proven: false,
        observed_at: event.observed_at.clone(),
        proof_issued_at: timestamp(pending.proof_issued_at).unwrap(),
        proof_expires_at: timestamp(pending.proof_expires_at).unwrap(),
        runtime_epoch_recorded: true,
        ended: false,
        terminalized_execution_instances: Vec::new(),
        released_current_claim: None,
    };
    assert!(pending.validate_receipt(&receipt).is_ok());
    receipt.proof_expires_at = timestamp(pending.proof_expires_at + 1).unwrap();
    assert!(pending.validate_receipt(&receipt).is_err());
    receipt.proof_expires_at = timestamp(pending.proof_expires_at).unwrap();
    receipt.observed_at = receipt.observed_at.replace('Z', ".5Z");
    assert!(pending.validate_receipt(&receipt).is_err());
    receipt.observed_at = event.observed_at.clone();
    receipt
        .terminalized_execution_instances
        .push(Uuid::from_u128(9).to_string());
    assert!(pending.validate_receipt(&receipt).is_err());
}

#[test]
fn closed_event_requires_nullable_fields() {
    let mut value = serde_json::to_value(start_event()).expect("event JSON");
    value.as_object_mut().unwrap().remove("signal");
    assert!(serde_json::from_value::<Event>(value).is_err());
}

#[test]
fn possibly_sent_end_keeps_its_original_proof_interval() {
    let mut event = start_event();
    event.phase = Phase::End;
    event.cause = Cause::Exited;
    event.exit_code = Some(0);
    event.absence_proven = true;
    let mut pending = PendingEvent::new(event, 1_791_158_405).expect("pending end");
    assert!(arm_end(&mut pending, 1_791_158_410).expect("arm end"));
    let original = pending.clone();
    assert!(!arm_end(&mut pending, 1_791_158_500).expect("read-only resolution"));
    assert_eq!(pending, original);
}

#[test]
fn manager_and_uid_evidence_are_strict() {
    let id = parse_manager_invocation("00000000000040008000000000000001").expect("manager ID");
    assert_eq!(id.to_string(), "00000000-0000-4000-8000-000000000001");
    assert!(parse_manager_invocation("00000000-0000-4000-8000-000000000001").is_err());
    assert!(parse_manager_invocation("00000000000000000000000000000000").is_err());
    assert!(status_has_uid("Name:\tprocess\nUid:\t1\t2\t3\t4\n", 3).unwrap());
    assert!(!status_has_uid("Name:\tprocess\nUid:\t1\t2\t3\t4\n", 5).unwrap());
    assert!(status_has_uid("Uid:\t1\t2\t3\n", 5).is_err());
    assert!(status_has_uid("Name:\tprocess\n", 5).is_err());
}

#[test]
fn inner_namespace_view_without_external_binding_cannot_prove_absence() {
    // The guard has no input accepting self/proc-1 equality as host proof.
    let error = require_uid_absent(65_534).expect_err("unbound view must fail closed");
    assert_eq!(
        error.to_string(),
        "lifecycle trusted manager and epoch containment binding unavailable"
    );
}

#[test]
fn cooperative_lock_without_admission_fence_cannot_prove_absence() {
    // Holding the launcher's lock supplies no external UID admission fence.
    let error = require_trusted_absence_binding().expect_err("cooperative lock is insufficient");
    assert_eq!(
        error.to_string(),
        "lifecycle trusted manager and epoch containment binding unavailable"
    );
}

#[test]
fn launcher_observation_is_not_an_end_publisher() {
    assert!(!is_stop_observer(Source::Launcher));
    assert!(is_stop_observer(Source::Systemd));
    // Eligibility alone does not establish the manager or cgroup boundary.
    assert!(require_trusted_absence_binding().is_err());
}

#[test]
fn end_authority_rejects_changed_protected_policy() {
    let original = b"synthetic original policy";
    let digest = wire::certificate_digest(original);
    assert!(require_policy_digest(original, &digest).is_ok());
    assert!(require_policy_digest(b"synthetic replacement policy", &digest).is_err());
}

#[test]
fn uncertain_post_reads_once_and_never_replays() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let address = listener.local_addr().expect("local address");
    let server = std::thread::spawn(move || {
        let mut methods = Vec::new();
        for index in 0..2 {
            let deadline = Instant::now() + Duration::from_secs(15);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("accept synthetic lifecycle request: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            reader.read_line(&mut first).unwrap();
            methods.push(first.split_whitespace().next().unwrap().to_string());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().unwrap();
                }
            }
            let mut body = vec![0_u8; length];
            reader.read_exact(&mut body).unwrap();
            let (status, response) = if index == 0 {
                ("503 Service Unavailable", "{}")
            } else {
                (
                    "200 OK",
                    "{\"status\":\"ok\",\"request_id\":\"fixture-read\",\"lifecycle_receipt\":null}",
                )
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
                response.len()
            )
            .unwrap();
        }
        methods
    });
    let now = super::super::api_claim::unix_seconds().expect("clock");
    let mut event = start_event();
    event.observed_at = timestamp(now).expect("timestamp");
    let pending = PendingEvent::new(event, now).expect("pending event");
    let result = deliver_once(
        &format!("http://{address}"),
        "synthetic-token",
        "synthetic-certificate",
        &[7; 32],
        &pending,
        true,
    )
    .expect("bounded readback");
    assert!(!result);
    assert_eq!(
        server.join().expect("server"),
        vec!["POST".to_string(), "GET".to_string()]
    );
}
