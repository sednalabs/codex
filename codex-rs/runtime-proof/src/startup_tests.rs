use super::*;
#[cfg(target_os = "linux")]
use base64::Engine;
#[cfg(target_os = "linux")]
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use pretty_assertions::assert_eq;

#[test]
fn descriptor_environment_accepts_only_an_index() {
    assert_eq!(
        parse_descriptor_index(std::ffi::OsStr::new("12")).unwrap(),
        12
    );
    assert!(parse_descriptor_index(std::ffi::OsStr::new("2")).is_err());
    assert!(parse_descriptor_index(std::ffi::OsStr::new("12x")).is_err());
    assert!(parse_descriptor_index(std::ffi::OsStr::new("-1")).is_err());
    assert!(parse_descriptor_index(std::ffi::OsStr::new("2147483648")).is_err());
}

#[cfg(target_os = "linux")]
fn seqpacket_pair() -> (File, File) {
    use std::os::fd::FromRawFd;
    let mut descriptors = [0; 2];
    assert_eq!(
        unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                0,
                descriptors.as_mut_ptr(),
            )
        },
        0
    );
    unsafe {
        (
            File::from_raw_fd(descriptors[0]),
            File::from_raw_fd(descriptors[1]),
        )
    }
}

#[cfg(target_os = "linux")]
fn bootstrap_frame(certificate: &[u8], seed: &[u8; SEED_BYTES]) -> Vec<u8> {
    let mut frame = Vec::new();
    frame.extend_from_slice(&(certificate.len() as u32).to_be_bytes());
    frame.extend_from_slice(certificate);
    frame.extend_from_slice(seed);
    frame
}

#[cfg(target_os = "linux")]
fn send_packet(file: &File, packet: &[u8]) {
    use std::os::fd::AsRawFd;
    assert_eq!(
        unsafe {
            libc::send(
                file.as_raw_fd(),
                packet.as_ptr().cast(),
                packet.len(),
                libc::MSG_NOSIGNAL,
            )
        },
        packet.len() as isize
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bootstrap_uses_root_peer_seqpacket_and_receives_one_bounded_frame() {
    use std::os::fd::AsRawFd;
    let (runtime, launcher) = seqpacket_pair();
    verify_sequenced_packet_socket(runtime.as_raw_fd()).unwrap();
    let peer = libc::ucred {
        pid: 1,
        uid: 0,
        gid: 0,
    };
    verify_root_peer_credentials(&peer).unwrap();
    let untrusted_peer = libc::ucred { uid: 1000, ..peer };
    assert!(verify_root_peer_credentials(&untrusted_peer).is_err());

    let certificate = b"compact-certificate";
    let seed = [41_u8; SEED_BYTES];
    send_packet(&launcher, &bootstrap_frame(certificate, &seed));
    let descriptor = runtime.as_raw_fd();
    let (received_certificate, received_seed) = read_bootstrap_frame(&runtime).unwrap();
    assert_eq!(received_certificate, "compact-certificate");
    assert_eq!(&*received_seed, &seed);
    assert_ne!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
}

#[cfg(target_os = "linux")]
#[test]
fn oversized_or_malformed_packet_is_rejected_and_owned_descriptor_closes() {
    use std::os::fd::AsRawFd;
    let (runtime, launcher) = seqpacket_pair();
    let oversized = vec![0; MAX_FRAME_BYTES + 1];
    send_packet(&launcher, &oversized);
    let descriptor = runtime.as_raw_fd();
    assert!(read_bootstrap_frame(&runtime).is_err());
    drop(runtime);
    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);

    let (runtime, launcher) = seqpacket_pair();
    send_packet(&launcher, &[0, 0, 0, 99, 1, 2, 3]);
    assert!(read_bootstrap_frame(&runtime).is_err());
}

#[cfg(target_os = "linux")]
#[test]
fn bootstrap_ack_is_one_exact_packet() {
    use std::os::fd::AsRawFd;
    let (runtime, launcher) = seqpacket_pair();
    complete_bootstrap_ack(&runtime).unwrap();
    let mut ack = [0_u8; 8];
    let read = unsafe { libc::recv(launcher.as_raw_fd(), ack.as_mut_ptr().cast(), ack.len(), 0) };
    assert_eq!(read, BOOTSTRAP_ACK.len() as isize);
    assert_eq!(&ack[..read as usize], BOOTSTRAP_ACK);
}

#[cfg(target_os = "linux")]
#[test]
fn acknowledgement_failure_erases_the_installed_signer() {
    let seed = Zeroizing::new([17_u8; SEED_BYTES]);
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let claims = crate::wire::CertificateClaims {
        version: 1,
        runtime_public_key: URL_SAFE_NO_PAD.encode(signing_key.verifying_key().as_bytes()),
        runtime_incarnation: uuid::Uuid::new_v4(),
        provider: "codex-native-runtime".to_string(),
        artifact_sha256: current_artifact_sha256().unwrap(),
        principal: uuid::Uuid::new_v4(),
        project_id: 1,
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
    let certificate_header = crate::wire::CertificateHeader {
        alg: "EdDSA".to_string(),
        typ: "runtime-certificate+jwt".to_string(),
        kid: "issuer-key".to_string(),
    };
    let certificate = compact_test_certificate(&certificate_header, &claims);
    crate::signer::install_bootstrap(certificate, seed).unwrap();

    let (runtime, launcher) = seqpacket_pair();
    drop(launcher);
    assert!(complete_bootstrap_ack(&runtime).is_err());
    assert_eq!(
        crate::signer::sign_claim_proof(
            &uuid::Uuid::new_v4(),
            "thread-evidence",
            "ops",
            Some("https://ops.example/mcp"),
            "work_item_claim",
            &serde_json::json!({}),
        )
        .unwrap(),
        None
    );
}

#[cfg(target_os = "linux")]
fn compact_test_certificate(
    header: &crate::wire::CertificateHeader,
    claims: &crate::wire::CertificateClaims,
) -> String {
    let header = serde_json::json!({
        "alg": header.alg,
        "typ": header.typ,
        "kid": header.kid,
    });
    let claims = serde_json::json!({
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
    format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header).unwrap()),
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap()),
        URL_SAFE_NO_PAD.encode([0_u8; 64]),
    )
}
