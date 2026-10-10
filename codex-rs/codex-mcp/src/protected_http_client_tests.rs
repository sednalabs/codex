//! Direct protected transport controls with imported synthetic runtime auth.

use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use codex_exec_server::ExecServerError;
use codex_exec_server::HttpClient;
use codex_exec_server::HttpRedirectPolicy;
use codex_exec_server::HttpRequestParams;
use codex_exec_server::HttpRequestResponse;
use codex_exec_server::HttpResponseBodyStream;
use futures::FutureExt;
use futures::future::BoxFuture;
use serde_json::json;

use crate::protected_http_client::ProtectedMcpHttpClient;

const BOOTSTRAP_FD: i32 = 3;
const AUTH_FD: i32 = 4;
const CHILD_MARKER: &str = "CODEX_RUNTIME_PROOF_DIRECT_SEND_CHILD";
const SERVER: &str = "ops";
const RECIPIENT: &str = "https://ops.example/mcp";
const PROVIDER_RECIPIENT: &str = "http://127.0.0.1:45678/v1";
const MCP_BEARER: &str = "runtime-proof-fixture-mcp-token-v1";

#[derive(Default)]
struct RecordingClient {
    requests: Mutex<Vec<(String, String, String)>>,
}

impl HttpClient for RecordingClient {
    fn http_request(
        &self,
        params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<HttpRequestResponse, ExecServerError>> {
        self.requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((
                params.method,
                params.url,
                params
                    .headers
                    .iter()
                    .find(|header| header.name.eq_ignore_ascii_case("authorization"))
                    .map(|header| header.value.clone())
                    .unwrap_or_default(),
            ));
        async { Err(ExecServerError::HttpRequest("fixture response".to_string())) }.boxed()
    }

    fn http_request_stream(
        &self,
        _params: HttpRequestParams,
    ) -> BoxFuture<'_, Result<(HttpRequestResponse, HttpResponseBodyStream), ExecServerError>> {
        async { Err(ExecServerError::HttpRequest("fixture response".to_string())) }.boxed()
    }
}

#[test]
#[ignore = "requires an explicitly invoked disposable Linux root fixture"]
fn retained_protected_http_client_rejects_a_send_after_imported_auth_expiry() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        run_protected_child().expect("protected direct-send child");
        return;
    }
    assert_eq!(unsafe { geteuid() }, 0, "fixture must run as root");
    launch_protected_child().expect("launch protected direct-send child");
}

fn run_protected_child() -> anyhow::Result<()> {
    codex_runtime_proof::startup::initialize_from_environment()?;
    let recorder = Arc::new(RecordingClient::default());
    let http = ProtectedMcpHttpClient::new(recorder.clone(), SERVER, RECIPIENT);
    let request = || HttpRequestParams {
        method: "POST".to_string(),
        url: RECIPIENT.to_string(),
        headers: Vec::new(),
        body: None,
        timeout_ms: Some(1000),
        redirect_policy: HttpRedirectPolicy::Follow,
        request_id: "synthetic-expiry-request".to_string(),
        stream_response: false,
    };

    let first = futures::executor::block_on(http.http_request(request()));
    anyhow::ensure!(
        first.is_err(),
        "recording transport should return its fixture error"
    );
    let first_count = recorder
        .requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .len();
    anyhow::ensure!(
        first_count == 1,
        "first protected send did not reach transport"
    );
    let sent = recorder
        .requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)[0]
        .clone();
    anyhow::ensure!(
        sent == (
            "POST".to_string(),
            RECIPIENT.to_string(),
            format!("Bearer {MCP_BEARER}")
        ),
        "first protected send did not use the imported selected credential"
    );

    let expiry = std::env::var("CODEX_RUNTIME_PROOF_FIXTURE_EXPIRY")?.parse::<u64>()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    if expiry >= now {
        std::thread::sleep(Duration::from_secs(expiry - now + 1));
    }
    let second = futures::executor::block_on(http.http_request(request()));
    anyhow::ensure!(
        second.is_err(),
        "expired protected client send must fail closed"
    );
    anyhow::ensure!(
        recorder
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
            == 1,
        "expired retained client reached its underlying network delegate"
    );
    Ok(())
}

fn launch_protected_child() -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let root = temp.path();
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o711))?;
    let home = root.join("home");
    std::fs::create_dir(&home)?;
    let config = format!(
        "model_provider = \"openai\"\ncli_auth_credentials_store = \"ephemeral\"\n\n[mcp_servers.ops]\nurl = \"{RECIPIENT}\"\n"
    );
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, config.as_bytes())?;
    std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o444))?;
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o555))?;

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let expires_at = now + 15;
    let executable = std::env::current_exe()?;
    let artifact_sha256 = sha256_file(&executable)?;
    let config_sha256 = sha256_file(&config_path)?;
    let mut seed = decode_hex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
    let public_key = decode_hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
    let claims = json!({
        "version": 1,
        "runtime_public_key": base64url(&public_key),
        "runtime_incarnation": "4be0643f-1d98-573b-97cd-ca98a65347dd",
        "provider": "codex-native-runtime",
        "artifact_sha256": artifact_sha256,
        "principal": "0f8fad5b-d9cb-469f-a165-70867728950e",
        "project_id": 1,
        "execution_lease_seconds": 300,
        "work_item_ref": "fixture-work-item",
        "recipient": RECIPIENT,
        "mcp_server": SERVER,
        "method": "tools/call",
        "tool": "work_item_claim",
        "purpose": "work-claim",
        "iat": now as i64,
        "exp": (now + 300) as i64
    });
    let certificate = compact_jws(
        &json!({"alg":"EdDSA","typ":"runtime-certificate+jwt","kid":"fixture-issuer"}),
        &claims,
        &[0_u8; 64],
    )?;
    let certificate_path = root.join("certificate.jwt");
    std::fs::write(&certificate_path, &certificate)?;
    let certificate_sha256 = format!("sha256:{}", sha256_file(&certificate_path)?);
    let auth = json!({
        "version": 1,
        "certificate_sha256": certificate_sha256,
        "runtime_incarnation": "4be0643f-1d98-573b-97cd-ca98a65347dd",
        "principal": "0f8fad5b-d9cb-469f-a165-70867728950e",
        "project_id": 1,
        "codex_home": home,
        "config_sha256": config_sha256,
        "mcp_server": SERVER,
        "recipient": RECIPIENT,
        "provider_recipient": PROVIDER_RECIPIENT,
        "expires_at": expires_at as i64,
        "provider": {
            "kind": "chatgpt_access_token",
            "credential_class": "synthetic_fixture",
            "access_token": synthetic_access_token(expires_at),
            "account_id": "00000000-0000-4000-8000-000000000001",
            "plan_type": "free"
        },
        "mcp_bearer": MCP_BEARER
    });

    let (bootstrap_parent, bootstrap_child) = seqpacket_pair()?;
    let (auth_parent, auth_child) = seqpacket_pair()?;
    let executable = std::env::current_exe()?;
    let bootstrap_fd = bootstrap_child.as_raw_fd();
    let auth_fd = auth_child.as_raw_fd();
    let mut command = Command::new(executable);
    command
        .env_clear()
        .env("CODEX_HOME", &home)
        .env(CHILD_MARKER, "1")
        .env("CODEX_RUNTIME_PROOF_FIXTURE_EXPIRY", expires_at.to_string())
        .env("OPS_RUNTIME_BOOTSTRAP_FD", BOOTSTRAP_FD.to_string())
        .env("OPS_RUNTIME_AUTH_FD", AUTH_FD.to_string())
        .env("PATH", "/usr/bin:/bin")
        .args([
            "--exact",
            "protected_http_client_tests::retained_protected_http_client_rejects_a_send_after_imported_auth_expiry",
            "--ignored",
            "--nocapture",
        ]);
    unsafe {
        command.pre_exec(move || {
            if dup2(bootstrap_fd, BOOTSTRAP_FD) < 0
                || fcntl(BOOTSTRAP_FD, F_SETFD, /*argument*/ 0) < 0
                || dup2(auth_fd, AUTH_FD) < 0
                || fcntl(AUTH_FD, F_SETFD, /*argument*/ 0) < 0
                || setgroups(/*size*/ 0, std::ptr::null()) != 0
                || setresgid(
                    /*real*/ 65534, /*effective*/ 65534, /*saved*/ 65534,
                ) != 0
                || setresuid(
                    /*real*/ 65534, /*effective*/ 65534, /*saved*/ 65534,
                ) != 0
                || prctl(
                    PR_SET_NO_NEW_PRIVS,
                    /*arg2*/ 1,
                    /*arg3*/ 0,
                    /*arg4*/ 0,
                    /*arg5*/ 0,
                ) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    drop(bootstrap_child);
    drop(auth_child);
    let mut bootstrap = bootstrap_frame(&certificate, &seed);
    send_packet(&bootstrap_parent, &bootstrap)?;
    bootstrap.fill(0);
    seed.fill(0);
    send_packet(&auth_parent, &serde_json::to_vec(&auth)?)?;
    let mut ack = [0_u8; 4];
    let mut bootstrap_parent = bootstrap_parent;
    bootstrap_parent.set_read_timeout(Some(Duration::from_secs(10)))?;
    if let Err(error) = bootstrap_parent.read_exact(&mut ack) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error.into());
    }
    if &ack != b"ACK1" {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("protected direct-send child did not ACK bootstrap");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::ensure!(
                status.success(),
                "protected direct-send fixture child failed: {status}"
            );
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait()?;
            anyhow::bail!("protected direct-send fixture child timed out");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

fn seqpacket_pair() -> anyhow::Result<(UnixStream, UnixStream)> {
    let mut descriptors = [-1; 2];
    if unsafe {
        socketpair(
            AF_UNIX,
            SOCK_SEQPACKET | SOCK_CLOEXEC,
            /*protocol*/ 0,
            descriptors.as_mut_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe {
        (
            UnixStream::from_raw_fd(descriptors[0]),
            UnixStream::from_raw_fd(descriptors[1]),
        )
    })
}

fn send_packet(socket: &UnixStream, bytes: &[u8]) -> anyhow::Result<()> {
    let sent = unsafe {
        send(
            socket.as_raw_fd(),
            bytes.as_ptr().cast(),
            bytes.len(),
            MSG_NOSIGNAL,
        )
    };
    anyhow::ensure!(
        sent >= 0 && sent as usize == bytes.len(),
        "send bootstrap fixture packet"
    );
    Ok(())
}

fn bootstrap_frame(certificate: &str, seed: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(4 + certificate.len() + seed.len());
    frame.extend_from_slice(&(certificate.len() as u32).to_be_bytes());
    frame.extend_from_slice(certificate.as_bytes());
    frame.extend_from_slice(seed);
    frame
}

fn compact_jws(
    header: &serde_json::Value,
    claims: &serde_json::Value,
    signature: &[u8],
) -> anyhow::Result<String> {
    Ok(format!(
        "{}.{}.{}",
        base64url(&serde_json::to_vec(header)?),
        base64url(&serde_json::to_vec(claims)?),
        base64url(signature),
    ))
}

fn synthetic_access_token(expires_at: u64) -> String {
    let header = br#"{"alg":"HS256","kid":"runtime-proof-fixture-only","typ":"JWT"}"#;
    let claims = format!(
        "{{\"aud\":\"runtime-proof-fixture-only\",\"exp\":{expires_at},\"https://api.openai.com/auth\":{{\"chatgpt_account_id\":\"00000000-0000-4000-8000-000000000001\",\"chatgpt_plan_type\":\"free\",\"chatgpt_user_id\":\"runtime-proof-fixture-user-v1\"}},\"iss\":\"https://runtime-proof-fixture.invalid\",\"sub\":\"runtime-proof-fixture-user-v1\"}}"
    );
    format!(
        "{}.{}.{}",
        base64url(header),
        base64url(claims.as_bytes()),
        base64url(b"not-a-real-signature-runtime-proof-fixture-v1"),
    )
}

fn sha256_file(path: &std::path::Path) -> anyhow::Result<String> {
    let output = Command::new("/usr/bin/sha256sum").arg(path).output()?;
    anyhow::ensure!(output.status.success(), "hash synthetic fixture input");
    Ok(String::from_utf8(output.stdout)?
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string())
}

fn base64url(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity((bytes.len() * 4).div_ceil(3));
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        encoded.push(TABLE[(first >> 2) as usize] as char);
        encoded.push(TABLE[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            encoded.push(TABLE[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        }
        if chunk.len() > 2 {
            encoded.push(TABLE[(third & 0x3f) as usize] as char);
        }
    }
    encoded
}

fn decode_hex(value: &str) -> Vec<u8> {
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => unreachable!("fixture hex constant"),
            };
            (digit(pair[0]) << 4) | digit(pair[1])
        })
        .collect()
}

use std::os::unix::fs::PermissionsExt;

const AF_UNIX: i32 = 1;
const SOCK_SEQPACKET: i32 = 5;
const SOCK_CLOEXEC: i32 = 0o2000000;
const F_SETFD: i32 = 2;
const PR_SET_NO_NEW_PRIVS: i32 = 38;
const MSG_NOSIGNAL: i32 = 0x4000;

unsafe extern "C" {
    fn geteuid() -> u32;
    fn socketpair(domain: i32, kind: i32, protocol: i32, descriptors: *mut i32) -> i32;
    fn dup2(old_fd: i32, new_fd: i32) -> i32;
    fn fcntl(fd: i32, command: i32, argument: i32) -> i32;
    fn setgroups(size: usize, groups: *const u32) -> i32;
    fn setresgid(real: u32, effective: u32, saved: u32) -> i32;
    fn setresuid(real: u32, effective: u32, saved: u32) -> i32;
    fn prctl(option: i32, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> i32;
    fn send(fd: i32, buffer: *const std::ffi::c_void, length: usize, flags: i32) -> isize;
}
