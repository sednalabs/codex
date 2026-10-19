//! Fixed local OAuth provisioning for the existing Ops API system credential.
//!
//! This path accepts no caller-selected OAuth, path, or command parameters. It
//! uses the exact API client contract, validates the returned access token's
//! fixed issuer/audience/scope/lifetime, and atomically replaces only the
//! volatile systemd credential source consumed by the fixed claim service.

use crate::api_claim;
use anyhow::Result;
use anyhow::bail;
use codex_rmcp_client::perform_fixed_ops_api_access_token_login;
use std::ffi::CStr;
use std::fs;
use std::fs::File;
use std::io::Read as _;
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

const BEARER_SOURCE_PATH: &str = "/run/credstore/ops-api-bearer";
const BEARER_LOCK_NAME: &str = ".codex-runtime-proof-api-bearer.lock";
const XDG_OPEN_PATH: &str = "/usr/bin/xdg-open";
const MAX_PASSWD_BUFFER: usize = 64 * 1024;

pub(crate) fn run() -> Result<i32> {
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
    let bearer =
        api_claim::validate_bearer_for_provisioning(access_token.access_token().to_string(), now)?;
    if bearer.expires_at <= now || bearer.expires_at > access_token.expires_at() {
        bail!("fixed Ops API access token expiry does not match its OAuth lifetime");
    }
    let credential = api_claim::encode_bearer_credential(&bearer.value)?;
    replace_bearer_source(Path::new(BEARER_SOURCE_PATH), &credential)
        .map_err(|_| anyhow::anyhow!("write fixed Ops API system credential"))?;
    Ok(0)
}

fn require_interactive_root_invocation() -> Result<()> {
    if unsafe { libc::geteuid() } != 0
        || invoking_uid_from_sudo_env().is_err()
        || unsafe { libc::isatty(libc::STDIN_FILENO) } != 1
    {
        bail!("fixed API bearer provisioning requires interactive sudo");
    }
    Ok(())
}

fn invoking_uid_from_sudo_env() -> Result<libc::uid_t> {
    let value =
        std::env::var("SUDO_UID").map_err(|_| anyhow::anyhow!("resolve the invoking sudo user"))?;
    parse_invoking_uid(&value)
}

fn parse_invoking_uid(value: &str) -> Result<libc::uid_t> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("resolve the invoking sudo user");
    }
    let uid = value
        .parse::<libc::uid_t>()
        .map_err(|_| anyhow::anyhow!("resolve the invoking sudo user"))?;
    if uid == 0 {
        bail!("fixed API bearer provisioning requires a non-root sudo caller");
    }
    Ok(uid)
}

fn invoking_user_identity() -> Result<(libc::uid_t, libc::gid_t, String)> {
    let uid = invoking_uid_from_sudo_env()?;
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
        if let Some(value) = std::env::var_os(name) {
            let value_bytes = value.as_os_str().as_bytes();
            if value_bytes.len() <= 4096 && !value_bytes.iter().any(u8::is_ascii_control) {
                command.env(name, value);
            }
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
    if parent != Path::new("/run/credstore") || parent.canonicalize()? != parent {
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

fn replace_bearer_source(path: &Path, credential: &[u8]) -> Result<()> {
    const FIXED_PARENT: &str = "/run/credstore";
    const FIXED_NAME: &str = "ops-api-bearer";
    if path != Path::new(BEARER_SOURCE_PATH) || path.parent() != Some(Path::new(FIXED_PARENT)) {
        bail!("Ops API credential target is not the fixed systemd input");
    }
    validate_fixed_parent(Path::new(FIXED_PARENT))?;

    let parent = open_fixed_directory(Path::new(FIXED_PARENT))?;
    validate_credential_store_directory(&parent, 0)?;

    let _lock = lock_credential_store(&parent, 0)?;
    let destination = std::ffi::CString::new(FIXED_NAME)?;
    atomic_replace_credential_at(&parent, &destination, credential, 0)
}

/// Removes the volatile source only when it still contains the exact bearer
/// that systemd copied into this unit's private credential directory.
pub(crate) fn cleanup_loaded_bearer_source(credentials_directory: &Path) -> Result<()> {
    let Some(loaded_bearer) = read_loaded_bearer(credentials_directory)? else {
        return Ok(());
    };
    let parent_path = Path::new("/run/credstore");
    validate_fixed_parent(parent_path)?;
    let parent = open_fixed_directory(parent_path)?;
    validate_credential_store_directory(&parent, 0)?;
    cleanup_bearer_source_at(&parent, &loaded_bearer, 0)
}

fn cleanup_bearer_source_at(
    parent: &File,
    loaded_bearer: &[u8],
    expected_owner: libc::uid_t,
) -> Result<()> {
    let _lock = lock_credential_store(parent, expected_owner)?;
    let source_name = std::ffi::CString::new("ops-api-bearer")?;
    let source_fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            source_name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if source_fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            bail!("volatile Ops API bearer source was not found");
        }
        return Err(error.into());
    }
    let source_file = unsafe { File::from_raw_fd(source_fd) };
    let source_metadata = source_file.metadata()?;
    validate_bearer_file_metadata(&source_metadata, expected_owner, Some(0o600))?;
    let source_bytes = zeroize::Zeroizing::new(crate::launcher::read_bounded(
        source_file,
        crate::api_claim::API_BEARER_CREDENTIAL_MAX_BYTES,
    )?);
    if source_bytes.as_slice() != loaded_bearer.as_slice() {
        bail!("volatile Ops API bearer source changed before cleanup");
    }

    let mut current = MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            source_name.as_ptr(),
            current.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            bail!("volatile Ops API bearer source changed before cleanup");
        }
        return Err(error.into());
    }
    let current = unsafe { current.assume_init() };
    if current.st_dev != source_metadata.dev() || current.st_ino != source_metadata.ino() {
        bail!("volatile Ops API bearer source changed before cleanup");
    }
    if unsafe { libc::unlinkat(parent.as_raw_fd(), source_name.as_ptr(), 0) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if unsafe { libc::fsync(parent.as_raw_fd()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn read_loaded_bearer(directory: &Path) -> Result<Option<zeroize::Zeroizing<Vec<u8>>>> {
    if !directory.is_absolute() || directory.canonicalize()? != directory {
        bail!("systemd credentials directory is not canonical and absolute");
    }
    let directory_metadata = fs::metadata(directory)?;
    if !directory_metadata.is_dir()
        || directory_metadata.uid() != 0
        || directory_metadata.mode() & 0o077 != 0
    {
        bail!("systemd credentials directory is not root-only");
    }
    let name = std::ffi::CString::new("ops-api-bearer")?;
    let directory_file = open_fixed_directory(directory)?;
    let fd = unsafe {
        libc::openat(
            directory_file.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    validate_bearer_file_metadata(&metadata, 0, None)?;
    Ok(Some(crate::launcher::read_bounded(
        file,
        crate::api_claim::API_BEARER_CREDENTIAL_MAX_BYTES,
    )?))
}

fn validate_bearer_file_metadata(
    metadata: &fs::Metadata,
    expected_owner: libc::uid_t,
    expected_mode: Option<u32>,
) -> Result<()> {
    let mode = metadata.mode() & 0o777;
    if !metadata.is_file()
        || metadata.uid() != expected_owner
        || (mode != 0o400 && mode != 0o600)
        || expected_mode.is_some_and(|expected| mode != expected)
        || metadata.len() == 0
        || metadata.len() > crate::api_claim::API_BEARER_CREDENTIAL_MAX_BYTES as u64
    {
        bail!("fixed Ops API credential is outside its protected file contract");
    }
    Ok(())
}

fn validate_credential_store_directory(
    directory: &File,
    expected_owner: libc::uid_t,
) -> Result<()> {
    let metadata = directory.metadata()?;
    if !metadata.is_dir() || metadata.uid() != expected_owner || metadata.mode() & 0o022 != 0 {
        bail!("fixed Ops API credential directory is not root controlled");
    }
    Ok(())
}

fn lock_credential_store(directory: &File, expected_owner: libc::uid_t) -> Result<File> {
    let lock_name = std::ffi::CString::new(BEARER_LOCK_NAME)?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            lock_name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let lock = unsafe { File::from_raw_fd(fd) };
    let metadata = lock.metadata()?;
    if !metadata.is_file() || metadata.uid() != expected_owner || metadata.mode() & 0o777 != 0o600 {
        bail!("fixed Ops API credential lock is outside its file contract");
    }
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(lock)
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
    if !metadata.is_file() || metadata.uid() != expected_owner || metadata.mode() & 0o777 != 0o600 {
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
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid fixed directory path",
        )
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
        assert!(
            replace_bearer_source(Path::new("/tmp/ops-api-bearer"), b"synthetic-token").is_err()
        );
    }

    #[test]
    fn sudo_invoker_uid_must_be_non_root_decimal() {
        assert_eq!(parse_invoking_uid("1000").ok(), Some(1000));
        assert!(parse_invoking_uid("0").is_err());
        assert!(parse_invoking_uid("+1000").is_err());
        assert!(parse_invoking_uid("1000\n").is_err());
    }

    #[test]
    fn credential_replacement_is_atomic_root_mode_and_fixed_to_one_basename() {
        let directory =
            std::env::temp_dir().join(format!("codex-runtime-proof-{}", Uuid::new_v4()));
        fs::create_dir(&directory).expect("create isolated credential directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("restrict isolated credential directory");
        let credential_path = directory.join("ops-api-bearer");
        fs::write(&credential_path, b"old-token").expect("create existing credential");
        fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o600))
            .expect("restrict existing credential");

        let parent = open_fixed_directory(&directory).expect("open credential directory");
        let name = std::ffi::CString::new("ops-api-bearer").expect("credential basename");
        atomic_replace_credential_at(&parent, &name, b"new-token", unsafe { libc::geteuid() })
            .expect("atomically replace credential");
        assert_eq!(
            fs::read(&credential_path).expect("read replacement"),
            b"new-token"
        );
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
        assert!(
            atomic_replace_credential_at(&parent, &traversal, b"must-not-write", unsafe {
                libc::geteuid()
            },)
            .is_err()
        );
        fs::remove_dir_all(directory).expect("remove isolated credential directory");
    }

    #[test]
    fn cleanup_removes_only_the_exact_loaded_credential_generation() {
        let directory =
            std::env::temp_dir().join(format!("codex-runtime-proof-{}", Uuid::new_v4()));
        fs::create_dir(&directory).expect("create isolated credential directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("restrict isolated credential directory");
        let credential_path = directory.join("ops-api-bearer");
        let token = crate::provider_auth::SecretString("same-token".to_string());
        let loaded =
            crate::api_claim::encode_bearer_credential(&token).expect("encode loaded credential");
        fs::write(&credential_path, &loaded).expect("write source credential");
        fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o600))
            .expect("restrict source credential");

        let parent = open_fixed_directory(&directory).expect("open credential directory");
        cleanup_bearer_source_at(&parent, &loaded, unsafe { libc::geteuid() })
            .expect("remove the credential systemd loaded");
        assert!(!credential_path.exists());
        fs::remove_dir_all(directory).expect("remove isolated credential directory");
    }

    #[test]
    fn cleanup_preserves_a_replacement_even_when_it_reuses_the_same_access_token() {
        let directory =
            std::env::temp_dir().join(format!("codex-runtime-proof-{}", Uuid::new_v4()));
        fs::create_dir(&directory).expect("create isolated credential directory");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("restrict isolated credential directory");
        let credential_path = directory.join("ops-api-bearer");
        let token = crate::provider_auth::SecretString("same-token".to_string());
        let loaded = crate::api_claim::encode_bearer_credential(&token)
            .expect("encode previously loaded credential");
        let replacement = crate::api_claim::encode_bearer_credential(&token)
            .expect("encode replacement credential");
        assert_ne!(loaded.as_slice(), replacement.as_slice());
        fs::write(&credential_path, &replacement).expect("write replacement credential");
        fs::set_permissions(&credential_path, fs::Permissions::from_mode(0o600))
            .expect("restrict replacement credential");

        let parent = open_fixed_directory(&directory).expect("open credential directory");
        assert!(cleanup_bearer_source_at(&parent, &loaded, unsafe { libc::geteuid() },).is_err());
        assert_eq!(
            fs::read(&credential_path).expect("read preserved replacement"),
            replacement.as_slice()
        );
        fs::remove_dir_all(directory).expect("remove isolated credential directory");
    }
}
