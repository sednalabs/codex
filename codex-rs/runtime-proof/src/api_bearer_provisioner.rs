//! Fixed local OAuth provisioning for the existing Ops API system credential.
//!
//! This path accepts no caller-selected OAuth, path, or command parameters. It
//! uses the exact API client contract, validates the returned access token's
//! fixed issuer/audience/scope/lifetime, and atomically replaces only the
//! volatile systemd credential source consumed by the fixed claim service.

use crate::api_claim;
use crate::launcher::ProvisioningStage;
use crate::launcher::provisioning_failure;
use anyhow::Result;
use anyhow::bail;
use codex_rmcp_client::FIXED_API_CLAIM_TIMEOUT_SECS;
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
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use uuid::Uuid;

const BEARER_SOURCE_PATH: &str = "/run/credstore/ops-api-bearer";
const BEARER_LOCK_NAME: &str = ".codex-runtime-proof-api-bearer.lock";
const XDG_OPEN_PATH: &str = "/usr/bin/xdg-open";
const MAX_PASSWD_BUFFER: usize = 64 * 1024;
const MAX_CALLBACK_URL_BYTES: usize = 64 * 1024;
const API_PROFILE_CHILD_ACTION: &str = "__runtime-proof-api-profile-child";
const API_PROFILE_CHANNEL_FD: libc::c_int = 3;
const API_TOKEN_FRAME_MAGIC: &[u8; 4] = b"R3A1";
const MAX_API_TOKEN_FRAME_BYTES: usize = 12 * 1024 + 12;
// Covers the fixed 300-second PKCE callback, existing 60-second credential
// lock, and bounded discovery/token requests with a small scheduling margin.
const API_PROFILE_CHILD_TIMEOUT: Duration = Duration::from_secs(400);
static CALLBACK_INPUT_INTERRUPTED: AtomicBool = AtomicBool::new(false);

const CALLBACK_INPUT_SIGNALS: [libc::c_int; 5] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGTSTP,
];

struct CallbackSignalGuard {
    previous: Vec<(libc::c_int, libc::sigaction)>,
}

impl CallbackSignalGuard {
    fn install() -> Result<Self> {
        CALLBACK_INPUT_INTERRUPTED.store(false, Ordering::Relaxed);
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = record_callback_input_signal as *const () as libc::sighandler_t;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };

        let mut guard = Self {
            previous: Vec::with_capacity(CALLBACK_INPUT_SIGNALS.len()),
        };
        for signal in CALLBACK_INPUT_SIGNALS {
            let mut previous = MaybeUninit::<libc::sigaction>::uninit();
            if unsafe { libc::sigaction(signal, &action, previous.as_mut_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            guard
                .previous
                .push((signal, unsafe { previous.assume_init() }));
        }
        Ok(guard)
    }

    fn restore(&mut self) -> std::io::Result<()> {
        while let Some((signal, previous)) = self.previous.last() {
            if unsafe { libc::sigaction(*signal, previous, std::ptr::null_mut()) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            self.previous.pop();
        }
        Ok(())
    }
}

impl Drop for CallbackSignalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

extern "C" fn record_callback_input_signal(_: libc::c_int) {
    CALLBACK_INPUT_INTERRUPTED.store(true, Ordering::Relaxed);
}

struct TerminalEchoGuard {
    fd: libc::c_int,
    original: libc::termios,
    active: bool,
}

impl TerminalEchoGuard {
    fn disable(fd: libc::c_int) -> Result<Self> {
        let mut original = MaybeUninit::<libc::termios>::uninit();
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let original = unsafe { original.assume_init() };
        let mut no_echo = original;
        no_echo.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &no_echo) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(Self {
            fd,
            original,
            active: true,
        })
    }

    fn restore(&mut self) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for TerminalEchoGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub(crate) fn run() -> Result<i32> {
    run_with_interaction(ProvisioningInteraction::Browser)
}

pub(crate) fn run_with_manual_callback_input() -> Result<i32> {
    run_with_interaction(ProvisioningInteraction::ManualCallbackInput)
}

enum ProvisioningInteraction {
    Browser,
    ManualCallbackInput,
}

fn run_with_interaction(interaction: ProvisioningInteraction) -> Result<i32> {
    require_interactive_root_invocation()
        .map_err(|_| provisioning_failure(ProvisioningStage::Precondition))?;
    let bearer_parent = Path::new(BEARER_SOURCE_PATH)
        .parent()
        .ok_or_else(|| provisioning_failure(ProvisioningStage::Precondition))?;
    validate_fixed_parent(bearer_parent)
        .map_err(|_| provisioning_failure(ProvisioningStage::Precondition))?;

    let access_token = obtain_access_token_as_invoking_user(interaction)?;

    let now = api_claim::unix_seconds()
        .map_err(|_| provisioning_failure(ProvisioningStage::TokenValidation))?;
    let bearer = api_claim::validate_bearer_for_provisioning(access_token.value.to_string(), now)
        .map_err(|_| provisioning_failure(ProvisioningStage::TokenValidation))?;
    let expected_principal = crate::launcher::expected_http_principal_if_configured()
        .map_err(|_| provisioning_failure(ProvisioningStage::TokenValidation))?;
    if bearer.expires_at <= now
        || bearer.expires_at > access_token.expires_at
        || expected_principal.is_some_and(|principal| principal != bearer.principal)
    {
        return Err(provisioning_failure(ProvisioningStage::TokenValidation));
    }
    let credential = api_claim::encode_bearer_credential(&bearer.value)
        .map_err(|_| provisioning_failure(ProvisioningStage::TokenValidation))?;
    replace_bearer_source(Path::new(BEARER_SOURCE_PATH), &credential)
        .map_err(|_| provisioning_failure(ProvisioningStage::CredentialWrite))?;
    Ok(0)
}

/// Entry point for the fixed unprivileged child. It requires an inherited
/// root-created seqpacket channel and writes only a bounded short-lived access
/// token frame; the refresh token remains in this user's OS keyring.
pub(crate) fn run_api_profile_child(use_manual_callback_input: bool) -> Result<i32> {
    let channel = inherited_api_profile_channel()
        .map_err(|_| provisioning_failure(ProvisioningStage::Precondition))?;
    verify_api_profile_parent(&channel)?;
    require_installed_launcher_image()
        .map_err(|_| provisioning_failure(ProvisioningStage::Precondition))?;
    let (uid, _, _) = invoking_user_identity()
        .map_err(|_| provisioning_failure(ProvisioningStage::Precondition))?;
    if uid == 0 || unsafe { libc::geteuid() } != uid {
        return Err(provisioning_failure(ProvisioningStage::Precondition));
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| provisioning_failure(ProvisioningStage::Runtime))?;
    let oauth_dispatch = tracing::Dispatch::none();
    let access_token = tracing::dispatcher::with_default(&oauth_dispatch, || {
        runtime.block_on(
            codex_rmcp_client::perform_fixed_ops_api_access_token_with_refresh(
                open_authorization_url_as_invoking_user,
                read_callback_url_from_invoking_terminal,
                use_manual_callback_input,
            ),
        )
    })
    .map_err(|error| provisioning_failure(error.stage().into()))?;

    send_api_token_frame(
        &channel,
        access_token.access_token(),
        access_token.expires_at(),
    )?;
    Ok(0)
}

fn inherited_api_profile_channel() -> Result<File> {
    if unsafe { libc::fcntl(API_PROFILE_CHANNEL_FD, libc::F_GETFD) } < 0 {
        bail!("fixed API credential child has no inherited channel");
    }
    Ok(unsafe { File::from_raw_fd(API_PROFILE_CHANNEL_FD) })
}

fn obtain_access_token_as_invoking_user(
    interaction: ProvisioningInteraction,
) -> Result<TransferredAccessToken> {
    let (uid, gid, home) = invoking_user_identity()
        .map_err(|_| provisioning_failure(ProvisioningStage::Precondition))?;
    let (parent_socket, child_socket) =
        seqpacket_pair().map_err(|_| provisioning_failure(ProvisioningStage::Runtime))?;
    let child_fd = child_socket.as_raw_fd();
    let parent_pid = unsafe { libc::getpid() };
    let mut command = Command::new(LAUNCHER_PATH);
    command
        .arg(API_PROFILE_CHILD_ACTION)
        .arg(match interaction {
            ProvisioningInteraction::Browser => "browser",
            ProvisioningInteraction::ManualCallbackInput => "callback-input",
        })
        .env_clear()
        .env("HOME", &home)
        .env("PATH", "/usr/bin:/bin")
        .env("SUDO_UID", uid.to_string())
        // The API refresh profile is global to this user's OS keyring. Keep
        // its existing OAuth lock in one stable per-user Codex home instead
        // of allowing different CODEX_HOME values to race the same keyring
        // refresh token.
        .env_remove("CODEX_HOME")
        .current_dir(&home)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for name in [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
    ] {
        if let Some(value) = std::env::var_os(name) {
            let value_bytes = value.as_os_str().as_bytes();
            if value_bytes.len() <= 4096 && !value_bytes.iter().any(u8::is_ascii_control) {
                command.env(name, value);
            }
        }
    }
    if let Some((runtime_directory, bus_address)) = fixed_user_bus(uid) {
        command
            .env("XDG_RUNTIME_DIR", runtime_directory)
            .env("DBUS_SESSION_BUS_ADDRESS", bus_address);
    }

    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0
                || libc::getppid() != parent_pid
                || libc::dup2(child_fd, API_PROFILE_CHANNEL_FD) < 0
                || libc::fcntl(API_PROFILE_CHANNEL_FD, libc::F_SETFD, 0) < 0
                || libc::setgroups(0, std::ptr::null()) != 0
                || libc::setresgid(gid, gid, gid) != 0
                || libc::setresuid(uid, uid, uid) != 0
                || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) != 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if libc::syscall(
                libc::SYS_close_range,
                (API_PROFILE_CHANNEL_FD + 1) as u32,
                u32::MAX,
                libc::CLOSE_RANGE_CLOEXEC,
            ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|_| provisioning_failure(ProvisioningStage::Runtime))?;
    drop(child_socket);

    let packet = receive_api_token_frame(&parent_socket);
    if packet.is_err() {
        // A bounded handoff timeout must not be followed by an unbounded wait
        // for an OAuth/keyring child that failed to produce its one result.
        let _ = child.kill();
    }
    let status = child
        .wait()
        .map_err(|_| provisioning_failure(ProvisioningStage::Runtime))?;
    if !status.success() {
        return Err(provisioning_failure(ProvisioningStage::OAuthExchange));
    }
    packet.map_err(|_| provisioning_failure(ProvisioningStage::OAuthExchange))
}

struct TransferredAccessToken {
    value: zeroize::Zeroizing<String>,
    expires_at: i64,
}

fn send_api_token_frame(channel: &File, token: &str, expires_at: i64) -> Result<()> {
    if token.is_empty() || token.len() > 12 * 1024 {
        bail!("fixed API access token is outside its handoff bounds");
    }
    let mut frame = zeroize::Zeroizing::new(Vec::with_capacity(12 + token.len()));
    frame.extend_from_slice(API_TOKEN_FRAME_MAGIC);
    frame.extend_from_slice(&expires_at.to_be_bytes());
    frame.extend_from_slice(token.as_bytes());
    let sent = unsafe {
        libc::send(
            channel.as_raw_fd(),
            frame.as_ptr().cast(),
            frame.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if sent < 0 || sent as usize != frame.len() {
        bail!("fixed API access token handoff failed");
    }
    Ok(())
}

fn receive_api_token_frame(channel: &File) -> Result<TransferredAccessToken> {
    let mut poll = libc::pollfd {
        fd: channel.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = i32::try_from(API_PROFILE_CHILD_TIMEOUT.as_millis()).unwrap_or(i32::MAX);
    let ready = unsafe { libc::poll(&mut poll, 1, timeout_ms) };
    if ready <= 0 || poll.revents & libc::POLLIN == 0 {
        bail!("fixed API credential child did not complete in time");
    }
    let mut frame = zeroize::Zeroizing::new(vec![0_u8; MAX_API_TOKEN_FRAME_BYTES]);
    let received = unsafe {
        libc::recv(
            channel.as_raw_fd(),
            frame.as_mut_ptr().cast(),
            frame.len(),
            libc::MSG_TRUNC,
        )
    };
    if received < 12 || received as usize > frame.len() {
        bail!("fixed API access token handoff has an invalid size");
    }
    decode_api_token_frame(&frame[..received as usize])
}

fn decode_api_token_frame(frame: &[u8]) -> Result<TransferredAccessToken> {
    if frame.len() < 12 || frame.len() > MAX_API_TOKEN_FRAME_BYTES {
        bail!("fixed API access token handoff has an invalid size");
    }
    if &frame[..4] != API_TOKEN_FRAME_MAGIC {
        bail!("fixed API access token handoff has an invalid format");
    }
    let expires_at = i64::from_be_bytes(frame[4..12].try_into()?);
    let token = std::str::from_utf8(&frame[12..])?;
    if token.is_empty() || token.len() > 12 * 1024 {
        bail!("fixed API access token handoff has an invalid token size");
    }
    Ok(TransferredAccessToken {
        value: zeroize::Zeroizing::new(token.to_string()),
        expires_at,
    })
}

fn verify_api_profile_parent(channel: &File) -> Result<()> {
    let mut socket_type: libc::c_int = 0;
    let mut socket_type_len = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut peer_len = std::mem::size_of_val(&peer) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            channel.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut socket_type_len,
        )
    } != 0
        || socket_type_len as usize != std::mem::size_of_val(&socket_type)
        || socket_type != libc::SOCK_SEQPACKET
        || unsafe {
            libc::getsockopt(
                channel.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut peer as *mut libc::ucred).cast(),
                &mut peer_len,
            )
        } != 0
        || peer_len as usize != std::mem::size_of_val(&peer)
        || peer.uid != 0
    {
        bail!("fixed API credential child requires its root launcher peer");
    }
    Ok(())
}

fn fixed_user_bus(uid: libc::uid_t) -> Option<(String, String)> {
    use std::os::unix::fs::FileTypeExt as _;

    let runtime_directory = format!("/run/user/{uid}");
    let runtime_path = Path::new(&runtime_directory);
    let runtime = fs::symlink_metadata(runtime_path).ok()?;
    if !runtime.is_dir() || runtime.uid() != uid || runtime.mode() & 0o077 != 0 {
        return None;
    }
    let bus_path = runtime_path.join("bus");
    let bus = fs::symlink_metadata(&bus_path).ok()?;
    if !bus.file_type().is_socket() || bus.uid() != uid {
        return None;
    }
    Some((
        runtime_directory.clone(),
        format!("unix:path={runtime_directory}/bus"),
    ))
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
    validate_fixed_authorization_url(authorization_url)?;

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
    let already_invoking_user =
        unsafe { libc::geteuid() } == uid && unsafe { libc::getegid() } == gid;
    unsafe {
        command.pre_exec(move || {
            if already_invoking_user {
                return Ok(());
            }
            if unsafe { libc::geteuid() } != 0 {
                return Err(std::io::Error::from_raw_os_error(libc::EPERM));
            }
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

fn read_callback_url_from_invoking_terminal(authorization_url: &str) -> Result<String> {
    validate_fixed_authorization_url(authorization_url)?;
    if unsafe { libc::isatty(libc::STDOUT_FILENO) } != 1 {
        bail!("manual OAuth callback input requires terminal output");
    }

    let mut signal_guard = CallbackSignalGuard::install()?;
    let mut echo_guard = TerminalEchoGuard::disable(libc::STDIN_FILENO)?;
    let result = read_callback_url_with_echo_disabled(authorization_url);
    echo_guard
        .restore()
        .map_err(|_| anyhow::anyhow!("restore invoking terminal settings"))?;
    signal_guard
        .restore()
        .map_err(|_| anyhow::anyhow!("restore invoking signal handlers"))?;
    let interruption = ensure_callback_input_not_interrupted();
    writeln!(std::io::stdout().lock())?;
    interruption?;
    result
}

fn read_callback_url_with_echo_disabled(authorization_url: &str) -> Result<String> {
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "Open this fixed authorization URL in your browser, complete sign-in, then paste the full callback URL below:\n{authorization_url}"
    )?;
    write!(stdout, "Callback URL: ")?;
    stdout.flush()?;

    let deadline = Instant::now() + Duration::from_secs(FIXED_API_CLAIM_TIMEOUT_SECS);
    let mut input = Vec::with_capacity(2048);
    loop {
        ensure_callback_input_not_interrupted()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("fixed OAuth callback timed out");
        }
        let timeout_ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: libc::STDIN_FILENO,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if ready == 0 {
            bail!("fixed OAuth callback timed out");
        }
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        let mut byte = [0_u8; 1];
        let read = unsafe { libc::read(libc::STDIN_FILENO, byte.as_mut_ptr().cast(), byte.len()) };
        if read < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        if read == 0 {
            if input.is_empty() {
                bail!("no OAuth callback URL was supplied");
            }
            break;
        }
        if byte[0] == b'\n' {
            break;
        }
        if input.len() == MAX_CALLBACK_URL_BYTES {
            bail!("OAuth callback URL exceeds 64 KiB");
        }
        input.push(byte[0]);
    }
    callback_url_from_terminal_input(input)
}

fn ensure_callback_input_not_interrupted() -> Result<()> {
    if CALLBACK_INPUT_INTERRUPTED.swap(false, Ordering::Relaxed) {
        bail!("manual OAuth callback input was interrupted");
    }
    Ok(())
}

fn callback_url_from_terminal_input(mut input: Vec<u8>) -> Result<String> {
    if input.last() == Some(&b'\r') {
        input.pop();
    }
    if input.is_empty() {
        bail!("no OAuth callback URL was supplied");
    }
    let callback_url =
        String::from_utf8(input).map_err(|_| anyhow::anyhow!("OAuth callback URL is invalid"))?;
    if callback_url.chars().any(char::is_control) {
        bail!("OAuth callback URL is invalid");
    }
    Ok(callback_url)
}

fn validate_fixed_authorization_url(authorization_url: &str) -> Result<url::Url> {
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
    Ok(url)
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
    validate_credential_store_directory(&parent, /*expected_owner*/ 0)?;

    let _lock = lock_credential_store(&parent, /*expected_owner*/ 0)?;
    let destination = std::ffi::CString::new(FIXED_NAME)?;
    atomic_replace_credential_at(&parent, &destination, credential, /*expected_owner*/ 0)
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
    validate_credential_store_directory(&parent, /*expected_owner*/ 0)?;
    cleanup_bearer_source_at(&parent, &loaded_bearer, /*expected_owner*/ 0)
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
    let source_bytes = crate::launcher::read_bounded(
        source_file,
        crate::api_claim::API_BEARER_CREDENTIAL_MAX_BYTES,
    )?;
    if source_bytes.as_slice() != loaded_bearer {
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
    validate_bearer_file_metadata(
        &metadata, /*expected_owner*/ 0, /*expected_mode*/ None,
    )?;
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
    fn api_profile_handoff_accepts_only_a_bounded_versioned_access_token_frame() {
        let mut frame = Vec::new();
        frame.extend_from_slice(API_TOKEN_FRAME_MAGIC);
        frame.extend_from_slice(&1_900_000_000_i64.to_be_bytes());
        frame.extend_from_slice(b"short-lived-access-token");

        let token = decode_api_token_frame(&frame).expect("decode fixed API handoff");
        assert_eq!(token.value.as_str(), "short-lived-access-token");
        assert_eq!(token.expires_at, 1_900_000_000);

        let mut wrong_version = frame.clone();
        wrong_version[0] = b'X';
        assert!(decode_api_token_frame(&wrong_version).is_err());
        assert!(decode_api_token_frame(&frame[..11]).is_err());

        let oversized = vec![0; MAX_API_TOKEN_FRAME_BYTES + 1];
        assert!(decode_api_token_frame(&oversized).is_err());
    }

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
    fn manual_callback_line_accepts_terminal_newline_and_rejects_control_input() {
        assert_eq!(
            callback_url_from_terminal_input(
                b"http://127.0.0.1:8765/callback/gGLT7yyzxEu8?code=fake&state=fake\r".to_vec()
            )
            .expect("decode CRLF terminal input"),
            "http://127.0.0.1:8765/callback/gGLT7yyzxEu8?code=fake&state=fake"
        );
        assert!(callback_url_from_terminal_input(b"url\nextra".to_vec()).is_err());
        assert!(callback_url_from_terminal_input(Vec::new()).is_err());
    }

    #[test]
    fn manual_callback_input_temporarily_disables_and_restores_terminal_echo() {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let _master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };

        let mut original = MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), original.as_mut_ptr()) },
            0
        );
        let mut original = unsafe { original.assume_init() };
        original.c_lflag |= libc::ECHO;
        assert_eq!(
            unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &original) },
            0
        );

        {
            let mut echo_guard =
                TerminalEchoGuard::disable(slave.as_raw_fd()).expect("disable terminal echo");
            let mut disabled = MaybeUninit::<libc::termios>::uninit();
            assert_eq!(
                unsafe { libc::tcgetattr(slave.as_raw_fd(), disabled.as_mut_ptr()) },
                0
            );
            let disabled = unsafe { disabled.assume_init() };
            assert_eq!(disabled.c_lflag & libc::ECHO, 0);
            assert_eq!(
                disabled.c_lflag & !libc::ECHO,
                original.c_lflag & !libc::ECHO
            );
            echo_guard.restore().expect("restore terminal echo");
        }

        let mut restored = MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), restored.as_mut_ptr()) },
            0
        );
        let restored = unsafe { restored.assume_init() };
        assert_eq!(restored.c_lflag, original.c_lflag);
    }

    #[test]
    fn manual_callback_interrupt_restores_terminal_echo() {
        let mut master = -1;
        let mut slave = -1;
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        let _master = unsafe { File::from_raw_fd(master) };
        let slave = unsafe { File::from_raw_fd(slave) };
        let mut original = MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), original.as_mut_ptr()) },
            0
        );
        let mut original = unsafe { original.assume_init() };
        original.c_lflag |= libc::ECHO;
        assert_eq!(
            unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &original) },
            0
        );

        let mut signal_guard = CallbackSignalGuard::install().expect("install cancel handlers");
        let mut echo_guard =
            TerminalEchoGuard::disable(slave.as_raw_fd()).expect("disable terminal echo");
        assert_eq!(
            unsafe { libc::raise(libc::SIGINT) },
            0,
            "deliver a controlled interrupt"
        );
        assert!(ensure_callback_input_not_interrupted().is_err());
        echo_guard
            .restore()
            .expect("restore terminal echo after interrupt");
        signal_guard
            .restore()
            .expect("restore prior signal handlers");

        let mut restored = MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(slave.as_raw_fd(), restored.as_mut_ptr()) },
            0
        );
        assert_ne!(unsafe { restored.assume_init() }.c_lflag & libc::ECHO, 0);
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
