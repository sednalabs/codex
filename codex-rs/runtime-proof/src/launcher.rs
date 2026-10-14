//! Fixed-policy root launcher for one protected Codex Work-task execution.
//!
//! The launcher accepts no command-line input. Its only policy is a root-owned
//! file at [`POLICY_PATH`], and its only credentials are fixed systemd
//! `LoadCredential` basenames. It starts the measured `codex-dev` ELF with the
//! existing runtime-proof bootstrap descriptors, then opens any credential
//! input only after `/proc/<pid>/exe` is bound to the already verified file
//! object.

use crate::auth::AuthFrame;
use crate::mcp_oauth::McpOAuthPolicy;
use crate::mcp_oauth::exchange_client_credentials;
use crate::provider_auth::ProviderAuth;
use crate::provider_auth::ProviderCredentialClass;
use crate::provider_auth::SecretString;
use crate::wire;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::Signer as _;
use ed25519_dalek::SigningKey;
use rand::RngCore as _;
use serde::Deserialize;
use sha2::Digest as _;
use sha2::Sha256;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::Write as _;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd as _;
use std::os::fd::RawFd;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Child;
use std::process::Command;
use std::process::ExitStatus;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use uuid::Uuid;
use zeroize::Zeroize as _;
use zeroize::Zeroizing;

const POLICY_PATH: &str = "/etc/codex/runtime-proof/claim-launch.json";
const CREDENTIALS_DIRECTORY_ENV: &str = "CREDENTIALS_DIRECTORY";
const ISSUER_SEED_CREDENTIAL: &str = "runtime-proof-issuer-seed";
const PROVIDER_AUTH_CREDENTIAL: &str = "provider-auth.json";
const MCP_CLIENT_SECRET_CREDENTIAL: &str = "ops-mcp-client-secret";
const RUNTIME_DIRECTORY: &str = "/run/codex-runtime-proof";
const PROVIDER_RECIPIENT: &str = "https://chatgpt.com/backend-api/codex";
const MCP_SERVER: &str = "ops";
const MCP_TOOL: &str = "work_item_claim";
const BOOTSTRAP_FD: RawFd = 3;
const AUTH_FD: RawFd = 4;
const CHILD_SOURCE_FD_MIN: RawFd = 32;
const RUNTIME_SEED_BYTES: usize = 32;
const MAX_POLICY_BYTES: usize = 32 * 1024;
const MAX_PROVIDER_AUTH_BYTES: usize = 24 * 1024;
const MAX_MCP_CLIENT_SECRET_BYTES: usize = 4096;
const MAX_EXECUTABLE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ACK_TIMEOUT: Duration = Duration::from_secs(60);
const READINESS_PACKET: &[u8; 4] = b"ACK1";

/// Root-owned policy for one fixed Work-task execution.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LauncherPolicy {
    version: u8,
    codex_dev_path: PathBuf,
    codex_dev_sha256: String,
    runtime_uid: u32,
    runtime_gid: u32,
    working_directory: PathBuf,
    principal: Uuid,
    project_id: u64,
    work_item_ref: String,
    mcp_recipient: String,
    mcp_oauth: McpOAuthPolicy,
    issuer_kid: String,
    issuer_public_key: String,
    execution_lease_seconds: u32,
    task_prompt: String,
}

struct VerifiedExecutable {
    file: File,
    device: u64,
    inode: u64,
    digest: String,
}

struct RunDirectory {
    codex_home: PathBuf,
    child_home: PathBuf,
    working_directory: PathBuf,
    config_sha256: String,
}

/// Starts the fixed root-owned task and returns the child exit code.
pub fn run() -> Result<i32> {
    reject_caller_input()?;
    require_root_launcher()?;

    let policy_bytes = read_root_file(Path::new(POLICY_PATH), MAX_POLICY_BYTES)?;
    let policy: LauncherPolicy =
        serde_json::from_slice(&policy_bytes).context("parse fixed launcher policy")?;
    validate_policy(&policy)?;

    let credential_directory = credential_directory()?;
    let runtime_directory = validate_runtime_directory(&policy)?;
    let working_directory = validate_working_directory(
        &policy.working_directory,
        policy.runtime_uid,
        policy.runtime_gid,
    )?;
    let executable = open_verified_executable(&policy)?;
    let run_directory = create_run_directory(&runtime_directory, &policy, working_directory)?;

    let (bootstrap_parent, bootstrap_child) = seqpacket_pair()?;
    let (auth_parent, auth_child) = seqpacket_pair()?;
    let bootstrap_source = duplicate_high(bootstrap_child.as_raw_fd())?;
    let auth_source = duplicate_high(auth_child.as_raw_fd())?;
    let executable_source = duplicate_high(executable.file.as_raw_fd())?;

    let mut child = spawn_verified_child(
        &executable_source,
        &bootstrap_source,
        &auth_source,
        &run_directory,
        &policy,
    )?;
    drop(bootstrap_child);
    drop(auth_child);
    drop(executable_source);
    drop(bootstrap_source);
    drop(auth_source);

    let result = supervise_child(
        &mut child,
        &executable,
        &bootstrap_parent,
        &auth_parent,
        &credential_directory,
        &run_directory,
        &policy,
    );
    if result.is_err() {
        terminate_child_group(&mut child);
    }
    result
}

fn reject_caller_input() -> Result<()> {
    if std::env::args_os().len() != 1 {
        bail!("launcher does not accept caller-selected arguments");
    }
    Ok(())
}

fn require_root_launcher() -> Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        bail!("fixed runtime-proof launcher must run as root");
    }
    Ok(())
}

fn validate_policy(policy: &LauncherPolicy) -> Result<()> {
    if policy.version != 1
        || policy.runtime_uid == 0
        || policy.runtime_gid == 0
        || policy.principal.is_nil()
        || policy.project_id == 0
        || policy.project_id > ((1_u64 << 53) - 1)
        || policy.work_item_ref.is_empty()
        || policy.work_item_ref.len() > 128
        || policy.work_item_ref.chars().any(char::is_control)
        || policy.execution_lease_seconds == 0
        || policy.execution_lease_seconds > wire::MAX_CERTIFICATE_SECONDS as u32
        || policy.task_prompt.trim().is_empty()
        || policy.task_prompt.len() > 16 * 1024
        || policy
            .task_prompt
            .chars()
            .any(|character| character.is_control() && character != '\n' && character != '\t')
    {
        bail!("fixed launcher policy is outside the admitted execution contract");
    }
    validate_sha256(&policy.codex_dev_sha256)?;
    if !policy.codex_dev_path.is_absolute()
        || policy.codex_dev_path.canonicalize()? != policy.codex_dev_path
    {
        bail!("fixed codex-dev executable path is not canonical and absolute");
    }
    if !policy.working_directory.is_absolute()
        || policy.working_directory.canonicalize()? != policy.working_directory
    {
        bail!("fixed task working directory is not canonical and absolute");
    }
    if policy.mcp_recipient.len() > 2048 {
        bail!("fixed MCP recipient exceeds its size limit");
    }
    validate_https_recipient(&policy.mcp_recipient)?;
    policy.mcp_oauth.validate()?;
    if policy.issuer_kid.is_empty()
        || policy.issuer_kid.len() > 128
        || !policy
            .issuer_kid
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        bail!("fixed issuer key identifier is invalid");
    }
    let issuer_public_key = URL_SAFE_NO_PAD.decode(&policy.issuer_public_key)?;
    if issuer_public_key.len() != 32
        || URL_SAFE_NO_PAD.encode(&issuer_public_key) != policy.issuer_public_key
    {
        bail!("fixed issuer public key has an invalid size");
    }
    Ok(())
}

fn credential_directory() -> Result<PathBuf> {
    let path = std::env::var_os(CREDENTIALS_DIRECTORY_ENV)
        .context("systemd credentials directory is unavailable")?;
    let path = PathBuf::from(path);
    if !path.is_absolute() || path.canonicalize()? != path {
        bail!("systemd credentials directory is not canonical and absolute");
    }
    validate_root_controlled_directory_tree(&path)?;
    let metadata = fs::metadata(&path)?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o077 != 0 {
        bail!("systemd credentials directory is not root-only");
    }
    Ok(path)
}

fn validate_runtime_directory(policy: &LauncherPolicy) -> Result<PathBuf> {
    let path = Path::new(RUNTIME_DIRECTORY);
    if path.canonicalize()? != path {
        bail!("runtime directory path is not canonical");
    }
    validate_root_controlled_directory_tree(path)?;
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != 0
        || metadata.gid() != policy.runtime_gid
        || metadata.mode() & 0o007 != 0
        || metadata.mode() & 0o710 != 0o710
        || metadata.mode() & 0o020 != 0
    {
        bail!("runtime directory does not match the fixed root/group custody");
    }
    Ok(path.to_path_buf())
}

fn validate_working_directory(path: &Path, runtime_uid: u32, runtime_gid: u32) -> Result<PathBuf> {
    validate_root_controlled_directory_tree(path)?;
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
        || path.canonicalize()? != path
    {
        bail!("fixed task working directory is not a root-controlled directory");
    }
    for directory in path.ancestors() {
        let metadata = fs::metadata(directory)?;
        let required_access = if directory == path { 0o005 } else { 0o001 };
        let accessible = if metadata.uid() == runtime_uid {
            metadata.mode() & (required_access << 6) == required_access << 6
        } else if metadata.gid() == runtime_gid {
            metadata.mode() & (required_access << 3) == required_access << 3
        } else {
            metadata.mode() & required_access == required_access
        };
        if !accessible {
            bail!("fixed task working directory is not accessible to the runtime identity");
        }
    }
    Ok(path.to_path_buf())
}

fn open_verified_executable(policy: &LauncherPolicy) -> Result<VerifiedExecutable> {
    validate_root_controlled_directory_tree(
        policy
            .codex_dev_path
            .parent()
            .context("fixed codex-dev path has no parent")?,
    )?;
    let file = open_nofollow(&policy.codex_dev_path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
        || metadata.mode() & 0o6000 != 0
        || metadata.len() == 0
        || metadata.len() > MAX_EXECUTABLE_BYTES
    {
        bail!("measured codex-dev image is not a root-owned executable");
    }
    let mut magic = [0_u8; 4];
    if unsafe { libc::pread(file.as_raw_fd(), magic.as_mut_ptr().cast(), magic.len(), 0) }
        != magic.len() as isize
        || magic != *b"\x7fELF"
    {
        bail!("measured codex-dev image is not a native ELF executable");
    }
    let digest = sha256_file(&file)?;
    if digest != policy.codex_dev_sha256 {
        bail!("measured codex-dev image digest differs from fixed policy");
    }
    Ok(VerifiedExecutable {
        file,
        device: metadata.dev(),
        inode: metadata.ino(),
        digest,
    })
}

fn create_run_directory(
    runtime_directory: &Path,
    policy: &LauncherPolicy,
    working_directory: PathBuf,
) -> Result<RunDirectory> {
    for _ in 0..8 {
        let path = runtime_directory.join(Uuid::new_v4().to_string());
        match fs::create_dir(&path) {
            Ok(()) => {
                return prepare_run_directory(
                    &path,
                    policy,
                    working_directory,
                    /*launcher_uid*/ 0,
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error).context("create fixed runtime directory"),
        }
    }
    bail!("could not allocate a unique protected runtime directory")
}

fn prepare_run_directory(
    path: &Path,
    policy: &LauncherPolicy,
    working_directory: PathBuf,
    launcher_uid: u32,
) -> Result<RunDirectory> {
    set_owner_and_mode(path, launcher_uid, policy.runtime_gid, /*mode*/ 0o710)?;
    let codex_home = path.join("codex-home");
    fs::create_dir(&codex_home)?;

    let config = render_codex_config(&policy.mcp_recipient);
    let config_path = codex_home.join("config.toml");
    write_owned_file(
        &config_path,
        config.as_bytes(),
        /*mode*/ 0o440,
        launcher_uid,
        policy.runtime_gid,
    )?;
    let config_sha256 = sha256_hex(config.as_bytes());

    let installation_id = codex_home.join("installation_id");
    write_owned_file(
        &installation_id,
        format!("{}\n", Uuid::new_v4()).as_bytes(),
        /*mode*/ 0o600,
        launcher_uid,
        policy.runtime_gid,
    )?;
    set_owner_and_mode(
        &installation_id,
        policy.runtime_uid,
        policy.runtime_gid,
        /*mode*/ 0o600,
    )?;
    set_owner_and_mode(
        &codex_home,
        launcher_uid,
        policy.runtime_gid,
        /*mode*/ 0o550,
    )?;

    let child_home = path.join("child-home");
    fs::create_dir(&child_home)?;
    for child_dir in ["tmp", "config", "cache", "data", "state", "runtime"] {
        let child_path = child_home.join(child_dir);
        fs::create_dir(&child_path)?;
        set_owner_and_mode(
            &child_path,
            policy.runtime_uid,
            policy.runtime_gid,
            /*mode*/ 0o700,
        )?;
    }
    set_owner_and_mode(
        &child_home,
        policy.runtime_uid,
        policy.runtime_gid,
        /*mode*/ 0o700,
    )?;

    Ok(RunDirectory {
        codex_home,
        child_home,
        working_directory,
        config_sha256,
    })
}

fn render_codex_config(mcp_recipient: &str) -> String {
    format!(
        "model_provider = \"openai\"\ncli_auth_credentials_store = \"ephemeral\"\napproval_policy = \"never\"\n\n[mcp_servers.ops]\nurl = {mcp_recipient:?}\nrequired = true\nenabled_tools = [\"work_item_claim\"]\n\n[mcp_servers.ops.tools.work_item_claim]\napproval_mode = \"approve\"\n"
    )
}

fn spawn_verified_child(
    executable_source: &File,
    bootstrap_source: &File,
    auth_source: &File,
    run_directory: &RunDirectory,
    policy: &LauncherPolicy,
) -> Result<Child> {
    let parent_pid = unsafe { libc::getpid() };
    let executable_source = executable_source.as_raw_fd();
    let bootstrap_source = bootstrap_source.as_raw_fd();
    let auth_source = auth_source.as_raw_fd();
    let mut command = Command::new(format!("/proc/self/fd/{executable_source}"));
    command
        .arg0("codex-dev")
        .args([
            "exec",
            "--json",
            "--ephemeral",
            "--strict-config",
            "--ignore-rules",
            "--skip-git-repo-check",
            "--sandbox",
            "read-only",
            "--cd",
        ])
        .arg(&run_directory.working_directory)
        .arg("-")
        .current_dir(&run_directory.working_directory)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "C.UTF-8")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("CODEX_HOME", &run_directory.codex_home)
        .env("HOME", &run_directory.child_home)
        .env("TMPDIR", run_directory.child_home.join("tmp"))
        .env("XDG_CONFIG_HOME", run_directory.child_home.join("config"))
        .env("XDG_CACHE_HOME", run_directory.child_home.join("cache"))
        .env("XDG_DATA_HOME", run_directory.child_home.join("data"))
        .env("XDG_STATE_HOME", run_directory.child_home.join("state"))
        .env("XDG_RUNTIME_DIR", run_directory.child_home.join("runtime"))
        .env("OPS_RUNTIME_BOOTSTRAP_FD", BOOTSTRAP_FD.to_string())
        .env("OPS_RUNTIME_AUTH_FD", AUTH_FD.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let runtime_uid = policy.runtime_uid;
    let runtime_gid = policy.runtime_gid;
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                || libc::getppid() != parent_pid
            {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(bootstrap_source, BOOTSTRAP_FD) < 0
                || libc::dup2(auth_source, AUTH_FD) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if libc::syscall(
                libc::SYS_close_range,
                5_u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            for descriptor in [BOOTSTRAP_FD, AUTH_FD] {
                if libc::fcntl(descriptor, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
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
    command.spawn().context("start fixed codex-dev task")
}

fn supervise_child(
    child: &mut Child,
    executable: &VerifiedExecutable,
    bootstrap_socket: &File,
    auth_socket: &File,
    credential_directory: &Path,
    run_directory: &RunDirectory,
    policy: &LauncherPolicy,
) -> Result<i32> {
    verify_child_image_and_identity(child, executable, policy)?;

    let issuer_seed = read_issuer_seed(credential_directory, policy)?;
    let mut runtime_seed = Zeroizing::new([0_u8; RUNTIME_SEED_BYTES]);
    rand::rng().fill_bytes(runtime_seed.as_mut());
    let provider = read_provider_auth(credential_directory)?;
    let client_secret = read_mcp_client_secret(credential_directory)?;
    let mcp_access_token = exchange_client_credentials(
        &policy.mcp_oauth,
        &client_secret,
        policy.principal,
        policy.project_id,
    )?;
    drop(client_secret);

    let (now, policy_expires_at, policy_task_deadline) = certificate_timing(policy)?;
    let (expires_at, execution_lease_seconds) = effective_expiry(
        now,
        policy_expires_at,
        mcp_access_token.expires_at,
    )?;
    let task_deadline = task_deadline_for_expiry(
        expires_at,
        policy_expires_at,
        policy_task_deadline,
    )?;

    let claims = wire::CertificateClaims {
        version: 1,
        runtime_public_key: URL_SAFE_NO_PAD.encode(
            SigningKey::from_bytes(&runtime_seed)
                .verifying_key()
                .as_bytes(),
        ),
        runtime_incarnation: Uuid::new_v4(),
        provider: "codex-native-runtime".to_string(),
        artifact_sha256: executable.digest.clone(),
        principal: policy.principal,
        project_id: policy.project_id,
        execution_lease_seconds,
        work_item_ref: policy.work_item_ref.clone(),
        recipient: policy.mcp_recipient.clone(),
        mcp_server: MCP_SERVER.to_string(),
        method: "tools/call".to_string(),
        tool: MCP_TOOL.to_string(),
        purpose: "work-claim".to_string(),
        iat: now,
        exp: expires_at,
    };
    let certificate = issue_certificate(&claims, &issuer_seed, policy)?;
    drop(issuer_seed);
    let certificate_sha256 = wire::certificate_digest(certificate.as_bytes());
    validate_live_credentials(&provider, &mcp_access_token.bearer, expires_at)?;

    let auth_packet = {
        let auth_frame = AuthFrame {
            version: 1,
            certificate_sha256,
            runtime_incarnation: claims.runtime_incarnation,
            principal: policy.principal,
            project_id: policy.project_id,
            codex_home: run_directory.codex_home.to_string_lossy().into_owned(),
            config_sha256: run_directory.config_sha256.clone(),
            mcp_server: MCP_SERVER.to_string(),
            recipient: policy.mcp_recipient.clone(),
            provider_recipient: PROVIDER_RECIPIENT.to_string(),
            expires_at,
            provider,
            mcp_bearer: mcp_access_token.bearer,
        };
        Zeroizing::new(
            serde_json::to_vec(&auth_frame).context("encode protected runtime auth frame")?,
        )
    };
    if auth_packet.len() > 32 * 1024 {
        bail!("protected runtime auth frame exceeds its size limit");
    }

    let bootstrap_packet = make_bootstrap_packet(&certificate, &runtime_seed)?;
    send_packet(bootstrap_socket, &bootstrap_packet)?;
    drop(bootstrap_packet);
    drop(runtime_seed);
    send_packet(auth_socket, &auth_packet)?;
    drop(auth_packet);
    let ack_timeout = ACK_TIMEOUT.min(task_deadline.saturating_duration_since(Instant::now()));
    receive_ack(bootstrap_socket, ack_timeout)?;
    if Instant::now() >= task_deadline {
        bail!("protected runtime expired before task execution started");
    }
    if let Some(mut stdin) = child.stdin.take() {
        let prompt = make_prompt(policy)?;
        write_prompt_until(&mut stdin, &prompt, task_deadline)?;
    } else {
        bail!("fixed codex-dev task has no prompt input");
    }
    let status = wait_for_child(child, task_deadline)?;
    Ok(exit_code(status))
}

fn effective_expiry(now: i64, policy_expires_at: i64, token_expires_at: i64) -> Result<(i64, u32)> {
    let expires_at = policy_expires_at.min(token_expires_at);
    let remaining = expires_at.saturating_sub(now);
    if remaining <= 0 {
        bail!("MCP client-credentials token expired before task admission");
    }
    let lease_seconds = u32::try_from(remaining)
        .context("effective runtime lease is outside the supported range")?;
    Ok((expires_at, lease_seconds))
}

fn task_deadline_for_expiry(
    expires_at: i64,
    policy_expires_at: i64,
    policy_task_deadline: Instant,
) -> Result<Instant> {
    let shortening_seconds = policy_expires_at
        .checked_sub(expires_at)
        .filter(|seconds| *seconds >= 0)
        .context("effective runtime expiry exceeds its fixed policy expiry")?;
    let shortening = Duration::from_secs(
        u64::try_from(shortening_seconds).context("effective runtime expiry is invalid")?,
    );
    policy_task_deadline
        .checked_sub(shortening)
        .context("effective runtime task deadline is outside the supported range")
}

fn verify_child_image_and_identity(
    child: &Child,
    executable: &VerifiedExecutable,
    policy: &LauncherPolicy,
) -> Result<()> {
    let proc_executable = PathBuf::from(format!("/proc/{}/exe", child.id()));
    let metadata = fs::metadata(proc_executable)?;
    if metadata.dev() != executable.device || metadata.ino() != executable.inode {
        bail!("started process differs from the measured codex-dev image");
    }
    let status = fs::read_to_string(format!("/proc/{}/status", child.id()))?;
    let mut uid = None;
    let mut gid = None;
    let mut groups = None;
    let mut effective_capabilities = None;
    let mut no_new_privileges = None;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("Uid:") {
            uid = Some(parse_identity_fields(value)?);
        } else if let Some(value) = line.strip_prefix("Gid:") {
            gid = Some(parse_identity_fields(value)?);
        } else if let Some(value) = line.strip_prefix("Groups:") {
            groups = Some(value.split_whitespace().count());
        } else if let Some(value) = line.strip_prefix("CapEff:") {
            effective_capabilities = Some(u64::from_str_radix(value.trim(), 16)?);
        } else if let Some(value) = line.strip_prefix("NoNewPrivs:") {
            no_new_privileges = Some(value.trim() == "1");
        }
    }
    let expected_uid = [policy.runtime_uid; 4];
    let expected_gid = [policy.runtime_gid; 4];
    if uid != Some(expected_uid)
        || gid != Some(expected_gid)
        || groups != Some(0)
        || effective_capabilities != Some(0)
        || no_new_privileges != Some(true)
    {
        bail!("started codex-dev process has an unexpected execution identity");
    }
    Ok(())
}

fn parse_identity_fields(value: &str) -> Result<[u32; 4]> {
    let values = value
        .split_whitespace()
        .map(str::parse::<u32>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    values
        .try_into()
        .map_err(|_| anyhow::anyhow!("process identity has an invalid shape"))
}

fn read_issuer_seed(directory: &Path, policy: &LauncherPolicy) -> Result<Zeroizing<[u8; 32]>> {
    let mut credential = read_credential(directory, ISSUER_SEED_CREDENTIAL, /*max_bytes*/ 32)?;
    if credential.len() != 32 {
        bail!("issuer signing credential has an invalid size");
    }
    let mut seed = Zeroizing::new([0_u8; 32]);
    seed.copy_from_slice(&credential);
    credential.zeroize();
    let key = SigningKey::from_bytes(&seed);
    let expected_public_key = URL_SAFE_NO_PAD.decode(&policy.issuer_public_key)?;
    if key.verifying_key().as_bytes().as_slice() != expected_public_key.as_slice() {
        bail!("issuer signing credential differs from the root policy pin");
    }
    Ok(seed)
}

fn read_provider_auth(directory: &Path) -> Result<ProviderAuth> {
    let bytes = read_credential(directory, PROVIDER_AUTH_CREDENTIAL, MAX_PROVIDER_AUTH_BYTES)?;
    serde_json::from_slice(&bytes).context("parse fixed provider-auth credential")
}

fn read_mcp_client_secret(directory: &Path) -> Result<SecretString> {
    let bytes = read_credential(
        directory,
        MCP_CLIENT_SECRET_CREDENTIAL,
        MAX_MCP_CLIENT_SECRET_BYTES,
    )?;
    let secret = std::str::from_utf8(&bytes)?.to_string();
    if secret.is_empty() || secret.chars().any(char::is_control) {
        bail!("fixed MCP OAuth client secret is invalid");
    }
    Ok(SecretString(secret))
}

fn validate_live_credentials(
    provider: &ProviderAuth,
    mcp_bearer: &SecretString,
    issued_until: i64,
) -> Result<()> {
    if provider.credential_class != ProviderCredentialClass::Live
        || provider.kind != "chatgpt_access_token"
        || provider.access_token.0.is_empty()
        || provider.access_token.0.len() > 16 * 1024
        || provider.access_token.0.chars().any(char::is_control)
        || provider.account_id.is_empty()
        || provider.account_id.len() > 512
        || provider.account_id.chars().any(char::is_control)
        || provider.plan_type.as_ref().is_none_or(|plan| {
            plan.is_empty() || plan.len() > 64 || plan.chars().any(char::is_control)
        })
        || mcp_bearer.0.len() > 16 * 1024
    {
        bail!("fixed provider-auth credential is outside the protected live contract");
    }
    crate::provider_auth::validate_provider_credential(
        provider,
        PROVIDER_RECIPIENT,
        &mcp_bearer.0,
        issued_until,
    )?;
    crate::provider_auth::ensure_protected_cloud_config_ineligible(provider.plan_type.as_deref())?;
    Ok(())
}

fn issue_certificate(
    claims: &wire::CertificateClaims,
    issuer_seed: &[u8; 32],
    policy: &LauncherPolicy,
) -> Result<String> {
    let header = wire::CertificateHeader {
        alg: "EdDSA".to_string(),
        typ: "runtime-certificate+jwt".to_string(),
        kid: policy.issuer_kid.clone(),
    };
    let encoded_header = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&header)?);
    let encoded_claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims)?);
    let signing_input = format!("{encoded_header}.{encoded_claims}");
    let issuer = SigningKey::from_bytes(issuer_seed);
    let signature = issuer.sign(signing_input.as_bytes());
    let certificate = format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    );
    if certificate.len() > wire::MAX_CERTIFICATE_BYTES {
        bail!("issued runtime certificate exceeds its size limit");
    }
    Ok(certificate)
}

fn make_bootstrap_packet(
    certificate: &str,
    seed: &[u8; RUNTIME_SEED_BYTES],
) -> Result<Zeroizing<Vec<u8>>> {
    if certificate.is_empty() || certificate.len() > wire::MAX_CERTIFICATE_BYTES {
        bail!("runtime certificate size is invalid");
    }
    let certificate_len = u32::try_from(certificate.len())?;
    let mut packet = Zeroizing::new(Vec::with_capacity(4 + certificate.len() + seed.len()));
    packet.extend_from_slice(&certificate_len.to_be_bytes());
    packet.extend_from_slice(certificate.as_bytes());
    packet.extend_from_slice(seed);
    Ok(packet)
}

fn read_credential(
    directory: &Path,
    basename: &str,
    max_bytes: usize,
) -> Result<Zeroizing<Vec<u8>>> {
    if basename.is_empty() || basename.contains('/') || basename.contains("..") {
        bail!("credential basename is invalid");
    }
    let path = directory.join(basename);
    let file = open_nofollow(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o077 != 0
        || metadata.len() == 0
        || metadata.len() > max_bytes as u64
    {
        bail!("fixed systemd credential is outside its protected file contract");
    }
    read_bounded(file, max_bytes)
}

fn read_root_file(path: &Path, max_bytes: usize) -> Result<Zeroizing<Vec<u8>>> {
    validate_root_controlled_directory_tree(
        path.parent().context("root policy path has no parent")?,
    )?;
    let file = open_nofollow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.mode() & 0o022 != 0
        || metadata.len() == 0
        || metadata.len() > max_bytes as u64
    {
        bail!("root policy file is outside its protected file contract");
    }
    read_bounded(file, max_bytes)
}

fn open_nofollow(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options
        .open(path)
        .with_context(|| format!("open fixed protected file {}", path.display()))
}

fn read_bounded(file: File, max_bytes: usize) -> Result<Zeroizing<Vec<u8>>> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(max_bytes.min(4096)));
    file.take(max_bytes as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        bail!("fixed protected input exceeds its size limit");
    }
    Ok(bytes)
}

fn validate_root_controlled_directory_tree(path: &Path) -> Result<()> {
    let canonical = path.canonicalize()?;
    if canonical != path {
        bail!("fixed protected directory path is not canonical");
    }
    for directory in path.ancestors() {
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!("fixed protected path has a non-root-controlled directory ancestor");
        }
    }
    Ok(())
}

fn write_owned_file(path: &Path, contents: &[u8], mode: u32, owner: u32, group: u32) -> Result<()> {
    let mut options = OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()?;
    set_owner_and_mode(path, owner, group, mode)
}

fn set_owner_and_mode(path: &Path, uid: u32, gid: u32, mode: u32) -> Result<()> {
    use std::os::unix::ffi::OsStrExt as _;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // Apply the mode before changing ownership. The launcher deliberately
    // does not retain CAP_FOWNER, so chmod after handing a path to the
    // unprivileged runtime identity would fail closed.
    if unsafe { libc::chmod(path.as_ptr(), mode as libc::mode_t) } != 0 {
        return Err(std::io::Error::last_os_error()).context("set fixed runtime permissions");
    }
    if unsafe { libc::chown(path.as_ptr(), uid, gid) } != 0 {
        return Err(std::io::Error::last_os_error()).context("set fixed runtime ownership");
    }
    Ok(())
}

fn seqpacket_pair() -> Result<(File, File)> {
    let mut descriptors = [0; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            descriptors.as_mut_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error()).context("create protected runtime channel");
    }
    Ok(unsafe {
        (
            File::from_raw_fd(descriptors[0]),
            File::from_raw_fd(descriptors[1]),
        )
    })
}

fn duplicate_high(descriptor: RawFd) -> Result<File> {
    let duplicate = unsafe { libc::fcntl(descriptor, libc::F_DUPFD_CLOEXEC, CHILD_SOURCE_FD_MIN) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error()).context("prepare fixed child descriptor");
    }
    Ok(unsafe { File::from_raw_fd(duplicate) })
}

fn send_packet(socket: &File, packet: &[u8]) -> Result<()> {
    let sent = unsafe {
        libc::send(
            socket.as_raw_fd(),
            packet.as_ptr().cast(),
            packet.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if sent < 0 {
        return Err(std::io::Error::last_os_error()).context("send protected runtime frame");
    }
    if sent as usize != packet.len() {
        bail!("protected runtime frame was not sent as one packet");
    }
    Ok(())
}

fn receive_ack(socket: &File, timeout: Duration) -> Result<()> {
    if timeout.is_zero() {
        bail!("protected runtime did not acknowledge startup before its deadline");
    }
    let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
    let mut descriptor = libc::pollfd {
        fd: socket.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
    if ready == 0 {
        bail!("protected runtime did not acknowledge startup before its deadline");
    }
    if ready < 0 {
        return Err(std::io::Error::last_os_error()).context("wait for protected runtime ACK1");
    }
    if descriptor.revents & libc::POLLIN == 0 {
        bail!("protected runtime channel closed before ACK1");
    }
    let mut ack = [0_u8; READINESS_PACKET.len() + 1];
    let received = unsafe {
        libc::recv(
            socket.as_raw_fd(),
            ack.as_mut_ptr().cast(),
            ack.len(),
            libc::MSG_TRUNC,
        )
    };
    if received != READINESS_PACKET.len() as isize
        || &ack[..READINESS_PACKET.len()] != READINESS_PACKET
    {
        bail!("protected runtime returned an invalid ACK1 packet");
    }
    Ok(())
}

fn write_prompt_until<W: std::io::Write + AsRawFd>(
    stdin: &mut W,
    prompt: &[u8],
    deadline: Instant,
) -> Result<()> {
    let descriptor = stdin.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error()).context("prepare fixed task prompt pipe");
    }

    let mut written = 0;
    while written < prompt.len() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("fixed task prompt exceeded its certificate lease");
        }
        let fractional_millisecond = if remaining.subsec_nanos().is_multiple_of(1_000_000) {
            0
        } else {
            1
        };
        let timeout_ms = remaining
            .as_millis()
            .saturating_add(fractional_millisecond)
            .clamp(1, i32::MAX as u128) as i32;
        let mut descriptor_state = libc::pollfd {
            fd: descriptor,
            events: libc::POLLOUT,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor_state, 1, timeout_ms) };
        if ready == 0 {
            continue;
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("wait for fixed task prompt pipe");
        }
        if descriptor_state.revents & libc::POLLOUT == 0 {
            bail!("fixed task prompt pipe closed before the prompt was sent");
        }
        match stdin.write(&prompt[written..]) {
            Ok(0) => bail!("fixed task prompt pipe closed before the prompt was sent"),
            Ok(count) => written += count,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error).context("write fixed task prompt"),
        }
    }
    Ok(())
}

fn wait_for_child(child: &mut Child, deadline: Instant) -> Result<ExitStatus> {
    loop {
        if Instant::now() >= deadline {
            bail!("fixed task exceeded its certificate execution lease");
        }
        let pid = child.id() as libc::pid_t;
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("observe fixed task termination");
            }
        } else if unsafe { info.si_pid() } == pid {
            terminate_child_group_before_reap(pid);
            return child.wait().context("reap fixed task");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn terminate_child_group(child: &mut Child) {
    let pid = child.id() as libc::pid_t;
    terminate_child_group_before_reap(pid);
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_child_group_before_reap(pid: libc::pid_t) {
    let process_group = -pid;
    unsafe {
        libc::kill(process_group, libc::SIGKILL);
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1))
}

fn issue_task_prompt(policy: &LauncherPolicy) -> String {
    format!(
        "The only authorized operation in this execution is claiming Work item {} in project {} as principal {}. Follow the fixed task instruction below and stop after this task.\n\n{}",
        policy.work_item_ref, policy.project_id, policy.principal, policy.task_prompt
    )
}

fn make_prompt(policy: &LauncherPolicy) -> Result<Zeroizing<Vec<u8>>> {
    let prompt = issue_task_prompt(policy);
    if prompt.len() > 20 * 1024 {
        bail!("fixed task prompt exceeds its size limit");
    }
    Ok(Zeroizing::new(prompt.into_bytes()))
}

fn validate_https_recipient(recipient: &str) -> Result<()> {
    let url = url::Url::parse(recipient).context("parse fixed MCP recipient")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.as_str() != recipient
    {
        bail!("fixed MCP recipient is not a canonical HTTPS URL");
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("fixed executable digest must be lowercase SHA-256 hex");
    }
    Ok(())
}

fn sha256_file(file: &File) -> Result<String> {
    let mut input = file.try_clone()?;
    input.rewind()?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn unix_seconds() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_secs() as i64)
}

fn certificate_timing(policy: &LauncherPolicy) -> Result<(i64, i64, Instant)> {
    let monotonic_start = Instant::now();
    let wall_now = SystemTime::now();
    let unix_now = wall_now
        .duration_since(UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?;
    let issued_at = i64::try_from(unix_now.as_secs())
        .context("system time does not fit the certificate timestamp")?;
    let expires_at = issued_at
        .checked_add(i64::from(policy.execution_lease_seconds))
        .context("certificate expiration timestamp overflowed")?;
    let expiry_wall = UNIX_EPOCH
        .checked_add(Duration::from_secs(expires_at as u64))
        .context("certificate expiration is outside the system time range")?;
    let remaining = expiry_wall
        .duration_since(wall_now)
        .context("certificate lease expired before it was issued")?;
    let task_deadline = monotonic_start
        .checked_add(remaining)
        .context("certificate task deadline overflowed")?;
    Ok((issued_at, expires_at, task_deadline))
}

#[cfg(test)]
#[path = "launcher_tests.rs"]
mod tests;
