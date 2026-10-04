use crate::provider_auth::ProviderAuth;
use crate::provider_auth::SecretString;
use crate::provider_auth::valid_provider_recipient;
use crate::provider_auth::validate_provider_credential;
use crate::signer;
use crate::wire;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use sha2::Digest as _;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Mutex;
use std::sync::OnceLock;
use zeroize::Zeroizing;

const AUTH_FD_ENV: &str = "OPS_RUNTIME_AUTH_FD";
const MAX_AUTH_FRAME_BYTES: usize = 32 * 1024;

struct BootstrapAuth {
    server: String,
    recipient: String,
    provider_recipient: String,
    expires_at: i64,
    bearer: Zeroizing<String>,
}

static BOOTSTRAP_AUTH: OnceLock<Mutex<Option<BootstrapAuth>>> = OnceLock::new();

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuthFrame {
    version: u8,
    certificate_sha256: String,
    runtime_incarnation: uuid::Uuid,
    principal: uuid::Uuid,
    project_id: u64,
    codex_home: String,
    config_sha256: String,
    mcp_server: String,
    recipient: String,
    provider_recipient: String,
    expires_at: i64,
    provider: ProviderAuth,
    mcp_bearer: SecretString,
}

#[cfg(target_os = "linux")]
pub(crate) fn initialize_from_environment(
    claims: &wire::CertificateClaims,
    certificate_sha256: &str,
) -> Result<()> {
    for name in [
        "OPENAI_API_KEY",
        "OPENAI_BASE_URL",
        "OPENAI_ORGANIZATION",
        "OPENAI_PROJECT",
        "CODEX_API_KEY",
        "CODEX_ACCESS_TOKEN",
    ] {
        if std::env::var_os(name).is_some() {
            bail!("protected runtime refuses ambient provider credential environment variables");
        }
    }
    let Some(fd_value) = std::env::var_os(AUTH_FD_ENV) else {
        bail!("protected runtime proof requires its matching auth descriptor");
    };
    unsafe { std::env::remove_var(AUTH_FD_ENV) };
    let fd = fd_value
        .to_str()
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .context("runtime auth descriptor index is invalid")?
        .parse::<i32>()
        .context("runtime auth descriptor index is out of range")?;
    if fd != 4 {
        bail!("runtime auth descriptor must be descriptor 4");
    }
    let socket = unsafe { std::fs::File::from_raw_fd(fd) };
    verify_root_seqpacket_peer(&socket)?;
    let mut frame_bytes = receive_auth_frame(&socket)?;
    let frame: AuthFrame =
        serde_json::from_slice(&frame_bytes).context("decode protected runtime auth frame")?;
    validate_and_import(frame, claims, certificate_sha256)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn initialize_from_environment(
    _claims: &wire::CertificateClaims,
    _certificate_sha256: &str,
) -> Result<()> {
    bail!("protected runtime authentication requires Linux")
}

fn validate_and_import(
    frame: AuthFrame,
    claims: &wire::CertificateClaims,
    certificate_sha256: &str,
) -> Result<()> {
    if frame.version != 1
        || frame.certificate_sha256 != certificate_sha256
        || frame.runtime_incarnation != claims.runtime_incarnation
        || frame.principal != claims.principal
        || frame.project_id != claims.project_id
        || frame.mcp_server != claims.mcp_server
        || frame.recipient != claims.recipient
        || !valid_provider_recipient(&frame.provider_recipient, &frame.provider.credential_class)
    {
        bail!("runtime auth frame does not match its issuer-bound certificate");
    }
    let now = unix_seconds()?;
    if frame.expires_at <= now || frame.expires_at > claims.exp {
        bail!("runtime auth frame is outside the certificate validity period");
    }
    if frame.provider.kind != "chatgpt_access_token"
        || frame.provider.access_token.0.is_empty()
        || frame.provider.access_token.0.len() > 16 * 1024
        || frame.provider.access_token.0.chars().any(char::is_control)
        || frame.provider.account_id.is_empty()
        || frame.provider.account_id.len() > 512
        || frame.provider.account_id.chars().any(char::is_control)
        || frame.provider.plan_type.as_ref().is_some_and(|plan| {
            plan.is_empty() || plan.len() > 64 || plan.chars().any(char::is_control)
        })
        || frame.mcp_bearer.0.is_empty()
        || frame.mcp_bearer.0.len() > 16 * 1024
        || frame.mcp_bearer.0.chars().any(char::is_control)
    {
        bail!("runtime auth frame contains invalid credential material");
    }
    validate_provider_credential(
        &frame.provider,
        &frame.provider_recipient,
        &frame.mcp_bearer.0,
        frame.expires_at,
    )?;

    let home = Path::new(&frame.codex_home).canonicalize()?;
    let environment_home = std::env::var_os("CODEX_HOME")
        .context("protected runtime requires its fixed CODEX_HOME")?;
    if Path::new(&environment_home).canonicalize()? != home {
        bail!("runtime auth CODEX_HOME differs from the configured runtime home");
    }
    require_root_read_only_directory(&home)?;
    let config_path = home.join("config.toml");
    let config_metadata = fs::metadata(&config_path)?;
    if config_metadata.uid() != 0 || config_metadata.mode() & 0o222 != 0 {
        bail!("protected runtime config must be root-owned and immutable");
    }
    let config_bytes = Zeroizing::new(fs::read(&config_path)?);
    if sha256_hex(&config_bytes) != frame.config_sha256 {
        bail!("runtime auth frame config digest does not match CODEX_HOME/config.toml");
    }
    validate_raw_config(&config_bytes, &frame.mcp_server, &frame.recipient)?;

    codex_login::auth::login_with_chatgpt_auth_tokens(
        &home,
        &frame.provider.access_token.0,
        &frame.provider.account_id,
        frame.provider.plan_type.as_deref(),
    )
    .context("import protected ChatGPT token into the ephemeral auth store")?;

    let auth = BootstrapAuth {
        server: frame.mcp_server,
        recipient: frame.recipient,
        provider_recipient: frame.provider_recipient,
        expires_at: frame.expires_at,
        bearer: Zeroizing::new(frame.mcp_bearer.0.clone()),
    };
    let store = BOOTSTRAP_AUTH.get_or_init(|| Mutex::new(None));
    let mut stored = store
        .lock()
        .map_err(|_| anyhow::anyhow!("protected runtime auth state is unavailable"))?;
    if stored.is_some() {
        bail!("protected runtime auth was initialized more than once");
    }
    *stored = Some(auth);
    Ok(())
}

fn validate_raw_config(config: &[u8], server: &str, recipient: &str) -> Result<()> {
    let config: toml::Value =
        toml::from_str(std::str::from_utf8(config)?).context("parse immutable runtime config")?;
    let table = config
        .as_table()
        .context("runtime config must be a TOML table")?;
    if table
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .is_some_and(|value| value != "openai")
        || table
            .get("cli_auth_credentials_store")
            .and_then(toml::Value::as_str)
            != Some("ephemeral")
        || table.get("model_providers").is_some()
        || table.get("chatgpt_base_url").is_some()
    {
        bail!(
            "protected runtime config must select the OpenAI provider and ephemeral auth storage"
        );
    }
    let mcp_servers = table
        .get("mcp_servers")
        .and_then(toml::Value::as_table)
        .context("protected runtime config lacks its selected MCP server")?;
    let selected = mcp_servers
        .get(server)
        .and_then(toml::Value::as_table)
        .context("protected runtime config does not contain the selected MCP server")?;
    if selected.len() != 1 || selected.get("url").and_then(toml::Value::as_str) != Some(recipient) {
        bail!("protected runtime MCP config must contain only its pinned URL");
    }
    if table.contains_key("env") || table.contains_key("dotenv") {
        bail!("protected runtime config may not select environment or dotenv credential sources");
    }
    Ok(())
}

fn require_root_read_only_directory(path: &Path) -> Result<()> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o222 != 0 {
        bail!("protected runtime home must be a root-owned immutable directory");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_root_seqpacket_peer(file: &std::fs::File) -> Result<()> {
    let fd = file.as_raw_fd();
    let mut socket_type: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut _).cast(),
            &mut size,
        )
    } != 0
        || size as usize != std::mem::size_of_val(&socket_type)
        || socket_type != libc::SOCK_SEQPACKET
    {
        bail!("runtime auth descriptor must be a sequenced-packet socket");
    }
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut peer_size = std::mem::size_of_val(&peer) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut _).cast(),
            &mut peer_size,
        )
    } != 0
        || peer_size as usize != std::mem::size_of_val(&peer)
        || peer.uid != 0
        || peer.pid <= 0
    {
        bail!("runtime auth peer must be the root launcher");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn receive_auth_frame(file: &std::fs::File) -> Result<Zeroizing<Vec<u8>>> {
    let mut frame = Zeroizing::new(vec![0_u8; MAX_AUTH_FRAME_BYTES + 1]);
    let mut control = [0_u8; 256];
    let mut vector = libc::iovec {
        iov_base: frame.as_mut_ptr().cast(),
        iov_len: frame.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len();
    let received = unsafe { libc::recvmsg(file.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(std::io::Error::last_os_error())
            .context("receive protected runtime auth frame");
    }
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 || message.msg_controllen != 0
    {
        bail!("runtime auth frame was truncated or carried ancillary data");
    }
    let received = received as usize;
    if received == 0 || received > MAX_AUTH_FRAME_BYTES {
        bail!("runtime auth frame has an invalid size");
    }
    frame.truncate(received);
    Ok(frame)
}

pub fn bearer_for_mcp(server: &str, recipient: &str) -> Result<Option<Zeroizing<String>>> {
    let Some(store) = BOOTSTRAP_AUTH.get() else {
        return Ok(None);
    };
    let mut stored = store
        .lock()
        .map_err(|_| anyhow::anyhow!("protected runtime auth state is unavailable"))?;
    ensure_auth_active(&mut stored)?;
    let Some(auth) = stored.as_ref() else {
        return Ok(None);
    };
    if server == auth.server && recipient == auth.recipient {
        return Ok(Some(Zeroizing::new(auth.bearer.to_string())));
    }
    if server == auth.server || recipient == auth.recipient {
        bail!("protected MCP auth target differs from its pinned server and URL");
    }
    Ok(None)
}

pub(crate) fn erase_auth_authority() -> Result<()> {
    let Some(store) = BOOTSTRAP_AUTH.get() else {
        return Ok(());
    };
    let mut stored = store
        .lock()
        .map_err(|_| anyhow::anyhow!("protected runtime auth state is unavailable"))?;
    *stored = None;
    Ok(())
}

pub fn is_protected_mcp_target(server: &str, recipient: &str) -> Result<bool> {
    let Some(store) = BOOTSTRAP_AUTH.get() else {
        return Ok(false);
    };
    let mut stored = store
        .lock()
        .map_err(|_| anyhow::anyhow!("protected runtime auth state is unavailable"))?;
    ensure_auth_active(&mut stored)?;
    let Some(auth) = stored.as_ref() else {
        return Ok(false);
    };
    if server == auth.server && recipient == auth.recipient {
        Ok(true)
    } else if server == auth.server || recipient == auth.recipient {
        bail!("protected MCP auth target differs from its pinned server and URL")
    } else {
        Ok(false)
    }
}

pub fn protected_mcp_target() -> Result<Option<(String, String)>> {
    let Some(store) = BOOTSTRAP_AUTH.get() else {
        return Ok(None);
    };
    let mut stored = store
        .lock()
        .map_err(|_| anyhow::anyhow!("protected runtime auth state is unavailable"))?;
    ensure_auth_active(&mut stored)?;
    let Some(auth) = stored.as_ref() else {
        return Ok(None);
    };
    Ok(Some((auth.server.clone(), auth.recipient.clone())))
}

pub fn protected_provider_recipient() -> Result<Option<String>> {
    let Some(store) = BOOTSTRAP_AUTH.get() else {
        return Ok(None);
    };
    let mut stored = store
        .lock()
        .map_err(|_| anyhow::anyhow!("protected runtime auth state is unavailable"))?;
    ensure_auth_active(&mut stored)?;
    Ok(stored.as_ref().map(|auth| auth.provider_recipient.clone()))
}

pub fn redact_mcp_bearer(
    server: &str,
    recipient: &str,
    value: &mut serde_json::Value,
) -> Result<()> {
    let Some(store) = BOOTSTRAP_AUTH.get() else {
        return Ok(());
    };
    let mut stored = store
        .lock()
        .map_err(|_| anyhow::anyhow!("protected runtime auth state is unavailable"))?;
    ensure_auth_active(&mut stored)?;
    let Some(auth) = stored.as_ref() else {
        return Ok(());
    };
    if server != auth.server || recipient != auth.recipient {
        if server == auth.server || recipient == auth.recipient {
            bail!("protected MCP redaction target differs from its pinned server and URL");
        }
        return Ok(());
    }
    redact_value(value, &auth.bearer);
    Ok(())
}

fn ensure_auth_active(stored: &mut Option<BootstrapAuth>) -> Result<()> {
    let Some(expires_at) = stored.as_ref().map(|auth| auth.expires_at) else {
        return Ok(());
    };
    if crate::startup::verify_runtime_protection().is_err() {
        *stored = None;
        signer::erase_signer_authority()?;
        bail!("protected runtime authentication expired or its protection changed");
    }
    if unix_seconds()? >= expires_at {
        *stored = None;
        signer::erase_signer_authority()?;
        bail!("protected runtime authentication expired or its protection changed");
    }
    Ok(())
}

fn redact_value(value: &mut serde_json::Value, secret: &str) {
    match value {
        serde_json::Value::String(text) => {
            if !secret.is_empty() {
                *text = text.replace(secret, "[runtime credential redacted]");
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                redact_value(value, secret);
            }
        }
        serde_json::Value::Object(values) => {
            for value in values.values_mut() {
                redact_value(value, secret);
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn unix_seconds() -> Result<i64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock precedes Unix epoch")?
        .as_secs() as i64)
}

fn sha256_hex(value: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for byte in sha2::Sha256::digest(value) {
        use std::fmt::Write as _;
        write!(out, "{byte:02x}").expect("writing to String cannot fail");
    }
    out
}

#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
