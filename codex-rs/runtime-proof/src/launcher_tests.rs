use super::*;
use ed25519_dalek::Signature;
use ed25519_dalek::Verifier as _;
use pretty_assertions::assert_eq;
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;
use std::time::Duration;
use std::time::Instant;

#[test]
fn api_bearer_provisioning_accepts_only_the_fixed_browser_modes() {
    let browser = vec![std::ffi::OsString::from("provision-api-bearer")];
    let manual_callback = vec![
        std::ffi::OsString::from("provision-api-bearer"),
        std::ffi::OsString::from("--no-browser"),
    ];
    let extra_argument = vec![
        std::ffi::OsString::from("provision-api-bearer"),
        std::ffi::OsString::from("--no-browser"),
        std::ffi::OsString::from("https://caller.example/callback"),
    ];

    assert!(matches_api_bearer_invocation(
        &browser,
        ApiBearerProvisioningMode::Browser
    ));
    assert!(matches_api_bearer_invocation(
        &manual_callback,
        ApiBearerProvisioningMode::ManualCallbackInput
    ));
    assert!(!matches_api_bearer_invocation(
        &manual_callback,
        ApiBearerProvisioningMode::Browser
    ));
    assert!(!matches_api_bearer_invocation(
        &extra_argument,
        ApiBearerProvisioningMode::ManualCallbackInput
    ));
    assert!(!matches_api_bearer_invocation(
        &extra_argument,
        ApiBearerProvisioningMode::Browser
    ));
}

#[test]
fn api_bearer_provisioning_failure_exposes_only_a_fixed_stage() {
    let error = provisioning_failure(ProvisioningStage::OAuthDiscovery);

    assert_eq!(
        safe_provisioning_failure_stage(&error),
        Some("oauth_discovery")
    );
    assert_eq!(
        format!("{error:#}"),
        "API bearer provisioning failed at oauth_discovery"
    );
    assert_eq!(
        safe_provisioning_failure_stage(&anyhow::anyhow!("sensitive internal detail")),
        None
    );
    assert_eq!(
        safe_provisioning_failure_detail(&anyhow::anyhow!("sensitive internal detail")),
        None
    );
}

#[test]
fn mcp_failure_stage_exposes_only_a_closed_phase() {
    let error = anyhow::anyhow!("private failure detail")
        .context(McpFailureStage::OAuth)
        .context("additional outer context");

    assert_eq!(safe_mcp_failure_stage(&error), Some("oauth"));
    assert_eq!(
        safe_mcp_failure_stage(&anyhow::anyhow!("MCP launcher failure stage: oauth")),
        None
    );
}

#[test]
fn callback_failure_keeps_its_stage_and_closed_detail() {
    let login_error = codex_rmcp_client::FixedOAuthLoginError::callback_failure(
        codex_rmcp_client::FixedOAuthCallbackFailure::InputTimeout,
    );
    let error = provisioning_oauth_failure(login_error);

    assert_eq!(
        safe_provisioning_failure_stage(&error),
        Some("oauth_callback")
    );
    assert_eq!(
        safe_provisioning_failure_detail(&error),
        Some("callback_input_timeout")
    );
    assert_eq!(
        format!("{error:#}"),
        "API bearer provisioning failed at oauth_callback (callback detail: callback_input_timeout)"
    );
}

#[test]
fn fixed_oauth_login_stages_map_to_provisioning_stages() {
    let stages = [
        (
            codex_rmcp_client::FixedOAuthLoginStage::Precondition,
            "precondition",
        ),
        (
            codex_rmcp_client::FixedOAuthLoginStage::Discovery,
            "oauth_discovery",
        ),
        (
            codex_rmcp_client::FixedOAuthLoginStage::Callback,
            "oauth_callback",
        ),
        (
            codex_rmcp_client::FixedOAuthLoginStage::Exchange,
            "oauth_exchange",
        ),
        (
            codex_rmcp_client::FixedOAuthLoginStage::TokenValidation,
            "token_validation",
        ),
        (
            codex_rmcp_client::FixedOAuthLoginStage::CredentialStore,
            "credential_store",
        ),
    ];

    for (oauth_stage, expected_stage) in stages {
        let error = provisioning_failure(oauth_stage.into());
        assert_eq!(
            safe_provisioning_failure_stage(&error),
            Some(expected_stage)
        );
    }
}

#[test]
fn installed_launcher_parent_validation_accepts_a_file_in_trusted_directories() {
    let (trust_boundary, launcher_path) = launcher_path_fixture();
    let expected_uid = fs::metadata(&trust_boundary)
        .expect("read trusted fixture directory metadata")
        .uid();

    assert!(
        fs::metadata(&launcher_path)
            .expect("read launcher fixture metadata")
            .is_file()
    );
    require_controlled_parent_directories(&launcher_path, &trust_boundary, expected_uid)
        .expect("validate trusted launcher parent directories");

    fs::remove_dir_all(trust_boundary).expect("remove trusted launcher fixture");
}

#[test]
fn installed_launcher_parent_validation_rejects_a_group_writable_parent() {
    let (trust_boundary, launcher_path) = launcher_path_fixture();
    let expected_uid = fs::metadata(&trust_boundary)
        .expect("read trusted fixture directory metadata")
        .uid();
    let launcher_parent = launcher_path
        .parent()
        .expect("launcher fixture has a parent directory");
    fs::set_permissions(launcher_parent, fs::Permissions::from_mode(0o771))
        .expect("make launcher fixture parent group writable");

    assert!(
        require_controlled_parent_directories(&launcher_path, &trust_boundary, expected_uid)
            .is_err()
    );

    fs::remove_dir_all(trust_boundary).expect("remove unsafe launcher fixture");
}

#[test]
fn installed_launcher_parent_validation_rejects_a_parent_without_invoker_traverse() {
    let (trust_boundary, launcher_path) = launcher_path_fixture();
    let expected_uid = fs::metadata(&trust_boundary)
        .expect("read trusted fixture directory metadata")
        .uid();
    let launcher_parent = launcher_path
        .parent()
        .expect("launcher fixture has a parent directory");
    fs::set_permissions(launcher_parent, fs::Permissions::from_mode(0o700))
        .expect("remove invoking-user traversal from launcher fixture parent");

    assert!(
        require_controlled_parent_directories(&launcher_path, &trust_boundary, expected_uid)
            .is_err()
    );

    fs::remove_dir_all(trust_boundary).expect("remove unsafe launcher fixture");
}

fn launcher_path_fixture() -> (PathBuf, PathBuf) {
    let trust_boundary = std::env::temp_dir().join(format!(
        "codex-runtime-proof-launcher-path-{}",
        Uuid::new_v4()
    ));
    fs::create_dir(&trust_boundary).expect("create trusted launcher fixture directory");
    fs::set_permissions(&trust_boundary, fs::Permissions::from_mode(0o711))
        .expect("make trusted launcher fixture traversable without listing or writes");

    let launcher_parent = trust_boundary.join("libexec");
    fs::create_dir(&launcher_parent).expect("create launcher parent fixture");
    fs::set_permissions(&launcher_parent, fs::Permissions::from_mode(0o711))
        .expect("make launcher parent fixture traversable without listing or writes");

    let launcher_path = launcher_parent.join("codex-runtime-proof-launcher");
    fs::write(&launcher_path, b"fixture").expect("create regular launcher file fixture");
    fs::set_permissions(&launcher_path, fs::Permissions::from_mode(0o755))
        .expect("make launcher fixture executable");

    (trust_boundary, launcher_path)
}

fn fixed_policy(seed: &[u8; RUNTIME_SEED_BYTES]) -> LauncherPolicy {
    let issuer = SigningKey::from_bytes(seed);
    LauncherPolicy {
        version: 2,
        principal: Uuid::from_u128(1),
        project_id: 17,
        work_item_ref: "w12345".to_string(),
        issuer_kid: "issuer-v1".to_string(),
        issuer_public_key: URL_SAFE_NO_PAD.encode(issuer.verifying_key().as_bytes()),
        execution_lease_seconds: 60,
        target: LauncherTarget::Mcp(McpLaunchConfig {
            codex_dev_path: PathBuf::from("/"),
            codex_dev_sha256: "a".repeat(64),
            runtime_uid: 65534,
            runtime_gid: 65534,
            working_directory: PathBuf::from("/"),
            mcp_recipient: "https://mcp.example/".to_string(),
            mcp_oauth: crate::mcp_oauth::McpOAuthPolicy {
                token_endpoint: "https://issuer.example/oauth/token".to_string(),
                expected_issuer: "https://issuer.example/".to_string(),
                client_id: "codex-work-claim".to_string(),
                audience: "https://mcp.example/".to_string(),
                requested_scope: "ops:write".to_string(),
            },
            task_prompt: "Claim the fixed Work item after checking its current state.".to_string(),
            lifecycle: None,
        }),
    }
}

#[test]
fn lifecycle_policy_is_opt_in_and_closed() {
    let mut value = serde_json::json!({
        "codex_dev_path": "/bin/example",
        "codex_dev_sha256": "a".repeat(64),
        "runtime_uid": 1001,
        "runtime_gid": 1001,
        "working_directory": "/",
        "mcp_recipient": "https://example.invalid",
        "mcp_oauth": {
            "token_endpoint": "https://example.invalid/token",
            "expected_issuer": "https://example.invalid",
            "client_id": "test",
            "audience": "test",
            "requested_scope": "ops:write"
        },
        "task_prompt": "test"
    });
    let original: McpLaunchConfig = serde_json::from_value(value.clone()).expect("ordinary MCP profile");
    assert!(original.lifecycle.is_none());
    value["lifecycle"] = serde_json::json!({"observer_sha256": "b".repeat(64)});
    let enabled: McpLaunchConfig = serde_json::from_value(value.clone()).expect("lifecycle profile");
    assert!(enabled.lifecycle.is_some());
    value["lifecycle"]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<McpLaunchConfig>(value).is_err());
}

#[test]
fn api_bearer_provisioning_binds_only_an_installed_http_policy_subject() {
    let seed = [9_u8; RUNTIME_SEED_BYTES];
    let mut policy = fixed_policy(&seed);
    let principal = policy.principal;
    assert!(http_policy_principal(&policy).is_err());

    policy.target = LauncherTarget::Http(ApiClaimConfig {
        launcher_sha256: "a".repeat(64),
        expected_generation: 1,
        expected_updated_at: "2026-10-22T00:00:00Z".to_string(),
    });
    assert_eq!(http_policy_principal(&policy).ok(), Some(principal));
}

fn fixed_claims() -> wire::CertificateClaims {
    wire::CertificateClaims {
        version: 2,
        runtime_public_key: URL_SAFE_NO_PAD.encode([4_u8; 32]),
        runtime_incarnation: Uuid::from_u128(2),
        provider: "codex-native-runtime".to_string(),
        artifact_sha256: "b".repeat(64),
        principal: Uuid::from_u128(1),
        project_id: 17,
        execution_lease_seconds: 60,
        work_item_ref: "w12345".to_string(),
        recipient: "https://mcp.example/".to_string(),
        mcp_server: Some(MCP_SERVER.to_string()),
        method: "tools/call".to_string(),
        tool: None,
        purpose: None,
        operations: Some(vec![
            wire::ProtectedOperation {
                tool: "work_item_claim".to_string(),
                purpose: "work-claim".to_string(),
            },
            wire::ProtectedOperation {
                tool: "work_item_release".to_string(),
                purpose: "work-release".to_string(),
            },
        ]),
        iat: 1_800_000_000,
        exp: 1_800_000_060,
    }
}

#[test]
fn effective_certificate_expiry_never_outlives_the_mcp_token() {
    let (expires_at, lease_seconds) = effective_expiry(
        /*now*/ 100, /*policy_expires_at*/ 1_000, /*token_expires_at*/ 400,
    )
    .expect("token expiry bounds the fixed task certificate");
    assert_eq!(expires_at, 400);
    assert_eq!(lease_seconds, 300);
    assert!(
        effective_expiry(
            /*now*/ 100, /*policy_expires_at*/ 1_000, /*token_expires_at*/ 100,
        )
        .is_err()
    );
}

#[test]
fn token_deadline_preserves_fractional_time_from_the_policy_clock_pair() {
    let monotonic_start = Instant::now();
    let policy_task_deadline = monotonic_start + Duration::from_millis(1_200);

    let token_task_deadline = task_deadline_for_expiry(
        /*expires_at*/ 101,
        /*policy_expires_at*/ 102,
        policy_task_deadline,
    )
    .expect("token expiry is earlier");

    assert_eq!(
        token_task_deadline.duration_since(monotonic_start),
        Duration::from_millis(200)
    );
    assert_eq!(
        task_deadline_for_expiry(
            /*expires_at*/ 102,
            /*policy_expires_at*/ 102,
            policy_task_deadline,
        )
        .expect("policy expiry keeps its deadline"),
        policy_task_deadline
    );
    assert!(
        task_deadline_for_expiry(
            /*expires_at*/ 103,
            /*policy_expires_at*/ 102,
            policy_task_deadline,
        )
        .is_err()
    );
}

#[test]
fn generated_claim_profile_matches_the_protected_runtime_admission() {
    let config = render_codex_config("https://mcp.example/");
    crate::auth::validate_raw_config(config.as_bytes(), MCP_SERVER, "https://mcp.example/")
        .expect("launcher profile is admitted by the child runtime");

    let unapproved = config.replace("approval_mode = \"approve\"", "approval_mode = \"prompt\"");
    assert!(
        crate::auth::validate_raw_config(
            unapproved.as_bytes(),
            MCP_SERVER,
            "https://mcp.example/",
        )
        .is_err()
    );
}

#[test]
fn issued_certificate_matches_the_existing_compact_jws_contract() {
    let issuer_seed = [7_u8; RUNTIME_SEED_BYTES];
    let policy = fixed_policy(&issuer_seed);
    let claims = fixed_claims();
    let certificate = issue_certificate(&claims, &issuer_seed, &policy)
        .expect("fixed issuer can sign the runtime certificate");

    let (header, parsed_claims) =
        wire::parse_certificate(&certificate).expect("certificate parses through the runtime seam");
    assert_eq!(header.alg, "EdDSA");
    assert_eq!(header.typ, "runtime-certificate+jwt");
    assert_eq!(header.kid, policy.issuer_kid);
    assert_eq!(
        serde_json::to_value(parsed_claims).expect("claims serialize"),
        serde_json::to_value(claims).expect("expected claims serialize")
    );

    let mut parts = certificate.split('.');
    let header = parts.next().expect("header segment");
    let claims = parts.next().expect("claims segment");
    let signature = URL_SAFE_NO_PAD
        .decode(parts.next().expect("signature segment"))
        .expect("signature is base64url encoded");
    assert!(parts.next().is_none());
    let issuer = SigningKey::from_bytes(&issuer_seed);
    issuer
        .verifying_key()
        .verify(
            format!("{header}.{claims}").as_bytes(),
            &Signature::from_slice(&signature).expect("Ed25519 signature has the fixed size"),
        )
        .expect("signature verifies against the root-pinned issuer key");
}

#[test]
fn bootstrap_packet_contains_only_the_length_prefixed_certificate_and_ephemeral_seed() {
    let certificate = "fixed-certificate";
    let seed = [9_u8; RUNTIME_SEED_BYTES];
    let packet = make_bootstrap_packet(certificate, &seed).expect("bounded bootstrap packet");
    let bytes = packet.as_slice();
    assert_eq!(
        u32::from_be_bytes(bytes[..4].try_into().expect("certificate length")) as usize,
        certificate.len()
    );
    assert_eq!(&bytes[4..4 + certificate.len()], certificate.as_bytes());
    assert_eq!(&bytes[4 + certificate.len()..], &seed);
}

#[test]
fn fixed_task_prompt_write_obeys_the_execution_deadline_under_backpressure() {
    let mut descriptors = [0; 2];
    assert_eq!(
        unsafe { libc::pipe2(descriptors.as_mut_ptr(), libc::O_CLOEXEC) },
        0,
        "create a private prompt pipe"
    );
    let _reader = unsafe { std::fs::File::from_raw_fd(descriptors[0]) };
    let mut writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    assert!(page_size > 0, "report the host page size");
    assert!(
        unsafe {
            libc::fcntl(
                writer.as_raw_fd(),
                libc::F_SETPIPE_SZ,
                page_size as libc::c_int,
            )
        } >= 0,
        "bound the pipe capacity for this backpressure regression"
    );

    let prompt = vec![b'x'; 1024 * 1024];
    let started = Instant::now();
    let result = write_prompt_until(&mut writer, &prompt, started + Duration::from_millis(100));

    assert!(
        result.is_err(),
        "a blocked prompt must expire with its task lease"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "prompt backpressure must not bypass the finite lease"
    );
}

#[test]
fn runtime_home_layout_preserves_child_write_and_protected_config_boundaries() {
    if let Some(base) = std::env::var_os("CODEX_RUNTIME_PROOF_LAYOUT_TEST_CHILD") {
        assert_runtime_home_access(&PathBuf::from(base));
        return;
    }

    let launcher_uid = unsafe { libc::geteuid() };
    let (runtime_uid, runtime_gid) = if launcher_uid == 0 {
        (65_534, 65_534)
    } else {
        (launcher_uid, unsafe { libc::getegid() })
    };
    let seed = [7_u8; RUNTIME_SEED_BYTES];
    let mut policy = fixed_policy(&seed);
    let LauncherTarget::Mcp(target) = &mut policy.target else {
        panic!("fixture selects the fixed MCP launcher target");
    };
    target.runtime_uid = runtime_uid;
    target.runtime_gid = runtime_gid;

    let base = std::env::temp_dir().join(format!(
        "codex-runtime-proof-layout-{}-{}",
        std::process::id(),
        Uuid::new_v4()
    ));
    fs::create_dir(&base).expect("create private layout test directory");
    fs::set_permissions(&base, fs::Permissions::from_mode(0o711))
        .expect("allow the runtime identity to traverse the fixture root");
    let run_path = base.join("run");
    fs::create_dir(&run_path).expect("create run directory before applying service modes");

    let run = prepare_run_directory(&run_path, target, PathBuf::from("/"), launcher_uid)
        .expect("prepare the same home layout used by the systemd service");

    assert_owned_mode(&run_path, launcher_uid, runtime_gid, /*mode*/ 0o710);
    assert_owned_mode(
        &run.codex_home,
        launcher_uid,
        runtime_gid,
        /*mode*/ 0o550,
    );
    let config = run.codex_home.join("config.toml");
    assert_owned_mode(&config, launcher_uid, runtime_gid, /*mode*/ 0o440);
    assert_eq!(
        sha256_hex(&fs::read(&config).expect("read protected generated config")),
        run.config_sha256
    );

    let installation_id = run.codex_home.join("installation_id");
    assert_owned_mode(
        &installation_id,
        runtime_uid,
        runtime_gid,
        /*mode*/ 0o600,
    );
    let installation_id_text = fs::read_to_string(&installation_id)
        .expect("the dropped child can open its pre-created installation ID");
    Uuid::parse_str(installation_id_text.trim()).expect("installation ID is a UUID");

    assert_owned_mode(
        &run.child_home,
        runtime_uid,
        runtime_gid,
        /*mode*/ 0o700,
    );
    for child_dir in ["tmp", "config", "cache", "data", "state", "runtime"] {
        let path = run.child_home.join(child_dir);
        assert_owned_mode(&path, runtime_uid, runtime_gid, /*mode*/ 0o700);
    }
    let mut child = Command::new("/proc/self/exe");
    child
        .args([
            "--exact",
            "launcher::tests::runtime_home_layout_preserves_child_write_and_protected_config_boundaries",
            "--nocapture",
            "--test-threads=1",
        ])
        .env_clear()
        .env("CODEX_RUNTIME_PROOF_LAYOUT_TEST_CHILD", &base)
        .env("CODEX_RUNTIME_PROOF_LAYOUT_CODEX_HOME", &run.codex_home)
        .env("CODEX_RUNTIME_PROOF_LAYOUT_CHILD_HOME", &run.child_home)
        .env("CODEX_RUNTIME_PROOF_LAYOUT_CONFIG", &config)
        .env(
            "CODEX_RUNTIME_PROOF_LAYOUT_INSTALLATION_ID",
            &installation_id,
        )
        .env("CODEX_RUNTIME_PROOF_LAYOUT_RUNTIME_TMP", run.child_home.join("tmp"));
    if launcher_uid == 0 {
        let runtime_uid = target.runtime_uid;
        let runtime_gid = target.runtime_gid;
        unsafe {
            child.pre_exec(move || {
                if libc::setgroups(0, std::ptr::null()) != 0
                    || libc::setresgid(runtime_gid, runtime_gid, runtime_gid) != 0
                    || libc::setresuid(runtime_uid, runtime_uid, runtime_uid) != 0
                    || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                    || libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let status = child
        .status()
        .expect("run the layout checks as the configured runtime identity");
    assert!(
        status.success(),
        "runtime identity home access checks failed"
    );
    assert_eq!(
        fs::read(run.child_home.join("tmp/permission-contract"))
            .expect("read test marker written by runtime identity"),
        b"child-writable".to_vec()
    );

    // Restore parent access after testing the intentionally immutable runtime
    // home; the test harness still owns and must remove the private fixture.
    fs::set_permissions(&run.codex_home, fs::Permissions::from_mode(/*mode*/ 0o700))
        .expect("restore fixture cleanup access to protected home");
    fs::set_permissions(&base, fs::Permissions::from_mode(/*mode*/ 0o700))
        .expect("restore fixture cleanup access to run root");
    fs::remove_dir_all(&base).expect("remove test runtime tree");
}

fn assert_runtime_home_access(base: &std::path::Path) {
    let codex_home = PathBuf::from(
        std::env::var_os("CODEX_RUNTIME_PROOF_LAYOUT_CODEX_HOME")
            .expect("protected CODEX_HOME path was supplied by the parent test"),
    );
    let child_home = PathBuf::from(
        std::env::var_os("CODEX_RUNTIME_PROOF_LAYOUT_CHILD_HOME")
            .expect("child HOME path was supplied by the parent test"),
    );
    let config = PathBuf::from(
        std::env::var_os("CODEX_RUNTIME_PROOF_LAYOUT_CONFIG")
            .expect("protected config path was supplied by the parent test"),
    );
    let installation_id = PathBuf::from(
        std::env::var_os("CODEX_RUNTIME_PROOF_LAYOUT_INSTALLATION_ID")
            .expect("installation ID path was supplied by the parent test"),
    );
    let runtime_tmp = PathBuf::from(
        std::env::var_os("CODEX_RUNTIME_PROOF_LAYOUT_RUNTIME_TMP")
            .expect("runtime temporary directory was supplied by the parent test"),
    );

    assert!(
        fs::read(&config).is_ok(),
        "runtime identity can read config"
    );
    assert!(
        Uuid::parse_str(
            fs::read_to_string(&installation_id)
                .expect("runtime identity can read installation ID")
                .trim()
        )
        .is_ok()
    );
    fs::write(runtime_tmp.join("permission-contract"), b"child-writable")
        .expect("runtime identity can write its temp area");
    assert!(
        fs::OpenOptions::new().write(true).open(&config).is_err(),
        "runtime identity cannot change its protected MCP configuration"
    );
    assert!(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(codex_home.join("unapproved"))
            .is_err(),
        "runtime identity cannot add settings under CODEX_HOME"
    );
    assert!(child_home.is_dir(), "runtime identity can access its HOME");
    assert!(base.is_dir(), "runtime identity can traverse its run root");
}

fn assert_owned_mode(path: &std::path::Path, uid: u32, gid: u32, mode: u32) {
    let metadata = fs::symlink_metadata(path).expect("inspect prepared path");
    assert!(metadata.is_file() || metadata.is_dir());
    assert_eq!(
        metadata.uid(),
        uid,
        "unexpected owner for {}",
        path.display()
    );
    assert_eq!(
        metadata.gid(),
        gid,
        "unexpected group for {}",
        path.display()
    );
    assert_eq!(
        metadata.mode() & 0o777,
        mode,
        "unexpected mode for {}",
        path.display()
    );
}

#[test]
fn launcher_policy_rejects_unsafe_project_ids_and_noncanonical_issuer_pins() {
    let seed = [7_u8; RUNTIME_SEED_BYTES];
    let mut policy = fixed_policy(&seed);
    validate_policy(&policy).expect("fixed canonical policy is accepted");

    policy.project_id = 1_u64 << 53;
    assert!(validate_policy(&policy).is_err());

    policy.project_id = 17;
    policy.issuer_public_key.push('=');
    assert!(validate_policy(&policy).is_err());
}

#[test]
fn fixed_http_policy_accepts_only_the_canonical_work_and_api_precondition() {
    let seed = [23_u8; RUNTIME_SEED_BYTES];
    let issuer = SigningKey::from_bytes(&seed);
    let policy_value = serde_json::json!({
        "version": 2,
        "principal": Uuid::from_u128(1),
        "project_id": 17,
        "work_item_ref": "w12345",
        "issuer_kid": "issuer-v1",
        "issuer_public_key": URL_SAFE_NO_PAD.encode(issuer.verifying_key().as_bytes()),
        "execution_lease_seconds": 60,
        "target": {
            "transport": "http",
            "config": {
                "launcher_sha256": "a".repeat(64),
                "expected_generation": 7,
                "expected_updated_at": "2026-10-04T05:00:00Z"
            }
        }
    });
    let mut policy: LauncherPolicy =
        serde_json::from_value(policy_value.clone()).expect("parse the closed API launcher policy");
    validate_policy(&policy).expect("the closed fixed API policy is valid");

    policy.work_item_ref = "example-work-item".to_string();
    assert!(validate_policy(&policy).is_err());

    for field in ["recipient", "mcp_server", "tool"] {
        let mut value = policy_value.clone();
        value["target"]["config"][field] = serde_json::json!("caller-selected");
        assert!(
            serde_json::from_value::<LauncherPolicy>(value).is_err(),
            "fixed API policy must reject caller-selected {field}"
        );
    }
}
