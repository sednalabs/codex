//! Fixed local OAuth provisioning for the existing Ops API system credential.
//!
//! This path accepts no caller-selected OAuth, path, or command parameters. It
//! uses the exact API client contract, verifies the resulting access token
//! against the root-owned claim policy, and atomically replaces only the
//! systemd credential source consumed by the fixed claim service.

use crate::api_claim;
use crate::provider_auth::SecretString;
use anyhow::Result;
use anyhow::bail;
use codex_rmcp_client::perform_fixed_ops_api_access_token_login;
use std::ffi::CStr;
use std::fs;
use std::fs::File;
use std::io::Write as _;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd as _;
use std::os::fd::FromRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use uuid::Uuid;

const BEARER_SOURCE_PATH: &str = "/etc/codex/runtime-proof/ops-api-bearer";
const XDG_OPEN_PATH: &str = "/usr/bin/xdg-open";
const MAX_PASSWD_BUFFER: usize = 64 * 1024;

pub(crate) fn run(principal: Uuid) -> Result<i32> {
    require_interactive_root_invocation()?;
    let bearer_parent = Path::new(BEARER_SOURCE_PATH)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("fixed Ops API credential path has no parent"))?;
    validate_fixed_parent(bearer_parent)?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| anyhow::anyhow!("build fixed OAuth provisioning runtime"))?;
    let oauth_dispatch = tracing::Dispatch::none();
    let access_token = tracing::dispatcher::with_default(&oauth_dispatch, || {
        runtime.block_on(perform_fixed_ops_api_access_token_login(
            open_authorization_url_as_invoking_user,
        ))
    })?;

    let now = api_claim::unix_seconds()?;
    let bearer = api_claim::validate_bearer(access_token.access_token().to_string(), principal, now)?;
    if bearer.expires_at <= now || bearer.expires_at > access_token.expires_at() {
        bail!("fixed Ops API access token expiry does not match its OAuth lifetime");
    }
    replace_bearer_source(Path::new(BEARER_SOURCE_PATH), &bearer.value)
        .map_err(|_| anyhow::anyhow!("write fixed Ops API system credential"))?;
    Ok(0)
}

fn require_interactive_root_invocation() -> Result<()> {
    if unsafe { libc::geteuid() } != 0
        || unsafe { libc::getuid() } == 0
        || unsafe { libc::isatty(libc::STDIN_FILENO) } != 1
    {
        bail!("fixed API bearer provisioning requires an interactive root invocation");
    }
    Ok(())
}

fn invoking_user_identity() -> Result<(libc::uid_t, libc::gid_t, String)> {
    let uid = unsafe { libc::getuid() };
    let mut buffer = vec![0_u8; MAX_PASSWD_BUFFER];
    let mut passwd = MaybeUninit::<libc::passwd>::uninit();
    let mut entry = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            passwd.as_mut_ptr(),
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut entry,
        )
    };
    if status != 0 || entry.is_null() {
        bail!("resolve the invoking local user for browser authorization");
    }
    let passwd = unsafe { passwd.assume_init_ref() };
    if passwd.pw_uid != uid || passwd.pw_name.is_null() || passwd.pw_dir.is_null() {
        bail!("the invoking local user account is incomplete");
    }
    let home = unsafe { CStr::from_ptr(passwd.pw_dir) }
        .to_str()
        .map_err(|_| anyhow::anyhow!("the invoking local home path is invalid"))?;
    if !Path::new(home).is_absolute() || home.chars().any(char::is_control) {
        bail!("the invoking local home path is outside its fixed contract");
    }
    Ok((uid, passwd.pw_gid, home.to_string()))
}

fn open_authorization_url_as_invoking_user(authorization_url: &str) -> Result<()> {
    let url = url::Url::parse(authorization_url)
        .map_err(|_| anyhow::anyhow!("fixed OAuth authorization URL is invalid"))?;
    if url.scheme() != "https"
        || url.host_str() != Some("auth.grantmatler.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!("fixed OAuth authorization URL is outside the trusted issuer");
    }

    let opener = Path::new(XDG_OPEN_PATH);
    let opener_metadata = fs::symlink_metadata(opener)?;
    if !opener_metadata.is_file()
        || opener_metadata.uid() != 0
        || opener_metadata.mode() & 0o022 != 0
        || opener_metadata.mode() & 0o111 == 0
    {
        bail!("fixed local browser opener is outside its root-owned contract");
    }

    let (uid, gid, home) = invoking_user_identity()?;
    let mut command = Command::new(opener);
    command
        .arg(authorization_url)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for name in [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_RUNTIME_DIR",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
        "DBUS_SESSION_BUS_ADDRESS",
    ] {
        if let Some(value) = std::env::var_os(name)
            && value.len() <= 4096
            && !value.as_os_str().as_bytes().iter().any(u8::is_ascii_control)
        {
            command.env(name, value);
        }
    }
    unsafe {
        command.pre_exec(move || {
            if libc::setgroups(0, std::ptr::null()) != 0
                || libc::setgid(gid) != 0
                || libc::setuid(uid) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let status = command
        .status()
        .map_err(|_| anyhow::anyhow!("start fixed local browser authorization"))?;
    if !status.success() {
        bail!("fixed local browser authorization could not be opened");
    }
    Ok(())
}

fn validate_fixed_parent(parent: &Path) -> Result<()> {
    if parent != Path::new("/etc/codex/runtime-proof") || parent.canonicalize()? != parent {
        bail!("fixed Ops API credential directory is not canonical");
    }
    for path in parent.ancestors() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            bail!("fixed Ops API credential directory is not root controlled");
        }
    }
    Ok(())
}

fn replace_bearer_source(path: &Path, bearer: &SecretString) -> Result<()> {
    const FIXED_PARENT: &str = "/etc/codex/runtime-proof";
    const FIXED_NAME: &str = "ops-api-bearer";
    if path != Path::new(BEARER_SOURCE_PATH) || path.parent() != Some(Path::new(FIXED_PARENT)) {
        bail!("Ops API credential target is not the fixed systemd input");
    }
    validate_fixed_parent(Path::new(FIXED_PARENT))?;

    let parent = open_fixed_directory(Path::new(FIXED_PARENT))?;
    let parent_metadata = parent.metadata()?;
    if !parent_metadata.is_dir()
        || parent_metadata.uid() != 0
        || parent_metadata.mode() & 0o022 != 0
    {
        bail!("fixed Ops API credential directory changed during provisioning");
    }

    let destination = std::ffi::CString::new(FIXED_NAME)?;
    atomic_replace_credential_at(&parent, &destination, bearer.0.as_bytes(), 0)
}

fn atomic_replace_credential_at(
    parent: &File,
    destination: &CStr,
    bytes: &[u8],
    expected_owner: libc::uid_t,
) -> Result<()> {
    let destination_bytes = destination.to_bytes();
    if destination_bytes.is_empty()
        || destination_bytes.contains(&b'/')
        || destination_bytes == b"."
        || destination_bytes == b".."
    {
        bail!("system credential filename is invalid");
    }
    let temporary_name = format!(".credential.{}.tmp", Uuid::new_v4());
    let temporary = std::ffi::CString::new(temporary_name.as_str())?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let temporary_file = unsafe { File::from_raw_fd(fd) };
    let write_result = write_and_commit_temporary(
        parent,
        &temporary_file,
        &temporary,
        destination,
        bytes,
        expected_owner,
    );
    drop(temporary_file);
    if write_result.is_err() {
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    write_result
}

fn write_and_commit_temporary(
    parent: &File,
    file: &File,
    temporary: &CStr,
    destination: &CStr,
    bytes: &[u8],
    expected_owner: libc::uid_t,
) -> Result<()> {
    if unsafe { libc::fchmod(file.as_raw_fd(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != expected_owner
        || metadata.mode() & 0o777 != 0o600
    {
        bail!("temporary Ops API credential is outside its fixed file contract");
    }
    let mut writer = file;
    writer.write_all(bytes)?;
    writer.sync_all()?;
    if unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            temporary.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { libc::fsync(parent.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn open_fixed_directory(path: &Path) -> std::io::Result<File> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid fixed directory path")
    })?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn bearer_source_requires_exact_fixed_path() {
        assert!(replace_bearer_source(
            Path::new("/tmp/ops-api-bearer"),
            &SecretString("synthetic-token".to_string())
        )
        .is_err());
    }

    #[test]
    fn credential_replacement_is_atomic_root_mode_and_fixed_to_one_basename() {
        let directory = std::env::temp_dir().join(format!("codex-runtime-proof-{}", Uuid::new_v4()));
        fs::create_dir(&directory).expect("create isolated credential directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("restrict isolated credential directory");
        let credential_path = directory.join("ops-api-bearer");
        fs::write(&credential_path, b"old-token").expect("create existing credential");
        fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o600))
            .expect("restrict existing credential");

        let parent = open_fixed_directory(&directory).expect("open credential directory");
        let name = std::ffi::CString::new("ops-api-bearer").expect("credential basename");
        atomic_replace_credential_at(
            &parent,
            &name,
            b"new-token",
            unsafe { libc::geteuid() },
        )
        .expect("atomically replace credential");
        assert_eq!(fs::read(&credential_path).expect("read replacement"), b"new-token");
        assert_eq!(
            fs::metadata(&credential_path)
                .expect("read replacement metadata")
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_dir(&directory)
                .expect("read credential directory")
                .count(),
            1
        );
        let traversal = std::ffi::CString::new("../other").expect("invalid basename fixture");
        assert!(atomic_replace_credential_at(
            &parent,
            &traversal,
            b"must-not-write",
            unsafe { libc::geteuid() },
        )
        .is_err());
        fs::remove_dir_all(directory).expect("remove isolated credential directory");
    }
}
