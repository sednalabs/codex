use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::fs::File;
use std::io::Read;
use zeroize::Zeroizing;

const DESCRIPTOR_ENV: &str = "OPS_RUNTIME_BOOTSTRAP_FD";
const AUTH_DESCRIPTOR_ENV: &str = "OPS_RUNTIME_AUTH_FD";
const MAX_FRAME_BYTES: usize = 20 * 1024;
const SEED_BYTES: usize = 32;
const BOOTSTRAP_ACK: &[u8; 4] = b"ACK1";

pub fn initialize_from_environment() -> Result<()> {
    let Some(descriptor) = std::env::var_os(DESCRIPTOR_ENV) else {
        if std::env::var_os(AUTH_DESCRIPTOR_ENV).is_some() {
            bail!("runtime auth descriptor cannot be used without runtime proof bootstrap");
        }
        return crate::signer::install_unconfigured();
    };
    let descriptor = parse_descriptor_index(&descriptor)?;
    if descriptor == 4 {
        bail!("runtime proof descriptor cannot alias the reserved auth descriptor");
    }
    // This is the synchronous first CLI entry; no threads or async runtime exist yet.
    unsafe { std::env::remove_var(DESCRIPTOR_ENV) };
    #[cfg(target_os = "linux")]
    {
        // Take ownership before any fallible checks so every refusal closes the FD.
        let bootstrap_file = unsafe { File::from_raw_fd(descriptor) };
        verify_bootstrap_socket(&bootstrap_file)?;
        verify_inherited_protection()?;
        set_non_dumpable()?;
        verify_runtime_protection()?;
        let (certificate, seed) = read_bootstrap_frame(&bootstrap_file)?;
        verify_runtime_protection()?;
        crate::signer::install_bootstrap(certificate, seed)?;
        let (claims, certificate_sha256) = crate::signer::certificate_for_auth()?;
        if let Err(error) = crate::auth::initialize_from_environment(&claims, &certificate_sha256) {
            crate::signer::erase_signer_authority()?;
            return Err(error).context("initialize protected runtime authentication");
        }
        if let Err(error) = verify_runtime_protection() {
            crate::auth::erase_auth_authority()?;
            crate::signer::erase_signer_authority()?;
            return Err(error);
        }
        complete_bootstrap_ack(&bootstrap_file)?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = descriptor;
        bail!("protected runtime proof requires Linux")
    }
}

fn parse_descriptor_index(descriptor: &std::ffi::OsStr) -> Result<i32> {
    descriptor
        .to_str()
        .filter(|value| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .context("runtime proof descriptor index is invalid")?
        .parse::<i32>()
        .context("runtime proof descriptor index is out of range")
        .and_then(|descriptor| {
            if descriptor < 3 {
                bail!("runtime proof descriptor must be a non-standard descriptor");
            }
            Ok(descriptor)
        })
}

pub(crate) fn verify_runtime_protection() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        verify_inherited_protection()?;
        if unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            bail!("runtime must remain non-dumpable");
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        bail!("protected runtime proof requires Linux");
    }
}

fn verify_inherited_protection() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut real_uid = 0;
        let mut effective_uid = 0;
        let mut saved_uid = 0;
        let mut real_gid = 0;
        let mut effective_gid = 0;
        let mut saved_gid = 0;
        if unsafe { libc::getresuid(&mut real_uid, &mut effective_uid, &mut saved_uid) } != 0
            || unsafe { libc::getresgid(&mut real_gid, &mut effective_gid, &mut saved_gid) } != 0
        {
            bail!("cannot verify inherited runtime identity");
        }
        if real_uid == 0
            || real_uid != effective_uid
            || real_uid != saved_uid
            || real_gid == 0
            || real_gid != effective_gid
            || real_gid != saved_gid
        {
            bail!("runtime must have one non-root real, effective and saved identity");
        }
        if unsafe { libc::getgroups(0, std::ptr::null_mut()) } != 0 {
            bail!("runtime must have no supplementary groups");
        }
        verify_capabilities()?;
        if unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } != 1 {
            bail!("runtime must inherit no_new_privs");
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        bail!("protected runtime proof requires Linux");
    }
}

#[cfg(target_os = "linux")]
fn verify_capabilities() -> Result<()> {
    #[repr(C)]
    struct CapabilityHeader {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Default)]
    struct CapabilityData {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }

    let mut header = CapabilityHeader {
        version: 0x2008_0522,
        pid: 0,
    };
    let mut data = [CapabilityData::default(), CapabilityData::default()];
    if unsafe {
        libc::syscall(
            libc::SYS_capget,
            &mut header as *mut CapabilityHeader,
            data.as_mut_ptr(),
        )
    } != 0
    {
        bail!("cannot verify runtime capability sets");
    }
    if data
        .iter()
        .any(|set| set.effective != 0 || set.permitted != 0 || set.inheritable != 0)
    {
        bail!("runtime capability sets must be empty");
    }
    let last_capability = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .context("cannot determine the kernel capability range")?
        .trim()
        .parse::<u32>()
        .context("kernel capability range is invalid")?;
    if last_capability >= 64 {
        bail!("kernel capability range exceeds the supported verifier");
    }
    for capability in 0..=last_capability {
        let present = unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_IS_SET,
                capability,
                0,
                0,
            )
        };
        if present == 1 {
            bail!("runtime ambient capabilities must be empty");
        }
        if present == -1 {
            return Err(std::io::Error::last_os_error())
                .context("cannot verify runtime ambient capabilities");
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_non_dumpable() -> Result<()> {
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0
        || unsafe { libc::prctl(libc::PR_GET_DUMPABLE, 0, 0, 0, 0) } != 0
    {
        bail!("cannot set and verify non-dumpable runtime state");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_non_dumpable() -> Result<()> {
    bail!("protected runtime proof requires Linux")
}

#[cfg(target_os = "linux")]
fn verify_bootstrap_socket(file: &File) -> Result<()> {
    use std::os::fd::AsRawFd;
    let descriptor = file.as_raw_fd();
    if descriptor < 3 {
        bail!("runtime proof descriptor must be an inherited non-standard descriptor");
    }
    verify_sequenced_packet_socket(descriptor)?;
    let mut peer: libc::ucred = unsafe { std::mem::zeroed() };
    let mut peer_size = std::mem::size_of_val(&peer) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut peer as *mut libc::ucred).cast(),
            &mut peer_size,
        )
    } != 0
        || peer_size as usize != std::mem::size_of_val(&peer)
    {
        bail!("cannot verify runtime proof bootstrap peer credentials");
    }
    verify_root_peer_credentials(&peer)
}

#[cfg(target_os = "linux")]
fn verify_sequenced_packet_socket(descriptor: std::os::fd::RawFd) -> Result<()> {
    let mut socket_type: libc::c_int = 0;
    let mut socket_type_size = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            descriptor,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut libc::c_int).cast(),
            &mut socket_type_size,
        )
    } != 0
        || socket_type_size as usize != std::mem::size_of_val(&socket_type)
        || socket_type != libc::SOCK_SEQPACKET
    {
        bail!("runtime proof descriptor must be a connected sequenced-packet socket");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn verify_root_peer_credentials(peer: &libc::ucred) -> Result<()> {
    if peer.pid <= 0 || peer.uid != 0 {
        bail!("runtime proof bootstrap peer must be the root launcher");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_bootstrap_frame(file: &File) -> Result<(String, Zeroizing<[u8; SEED_BYTES]>)> {
    use std::os::fd::AsRawFd;
    let mut frame_buffer = Zeroizing::new([0_u8; MAX_FRAME_BYTES + 1]);
    let mut vector = libc::iovec {
        iov_base: frame_buffer.as_mut_ptr().cast(),
        iov_len: frame_buffer.len(),
    };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    message.msg_iov = &mut vector;
    message.msg_iovlen = 1;
    let received = unsafe { libc::recvmsg(file.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
    if received < 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot receive runtime proof bootstrap frame");
    }
    if message.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        bail!("runtime proof bootstrap packet was truncated");
    }
    let frame_len = received as usize;
    if !(4 + SEED_BYTES..=MAX_FRAME_BYTES).contains(&frame_len) {
        bail!("runtime proof bootstrap frame has an invalid size");
    }
    let frame = &frame_buffer[..frame_len];
    let certificate_len = u32::from_be_bytes(frame[..4].try_into()?) as usize;
    if certificate_len == 0
        || certificate_len > crate::wire::MAX_CERTIFICATE_BYTES
        || frame.len() != 4 + certificate_len + SEED_BYTES
    {
        bail!("runtime proof bootstrap frame has an invalid length");
    }
    let certificate = std::str::from_utf8(&frame[4..4 + certificate_len])?.to_string();
    let mut seed = Zeroizing::new([0u8; SEED_BYTES]);
    seed.copy_from_slice(&frame[4 + certificate_len..]);
    Ok((certificate, seed))
}

#[cfg(target_os = "linux")]
fn send_bootstrap_ack(file: &File) -> Result<()> {
    use std::os::fd::AsRawFd;
    let sent = unsafe {
        libc::send(
            file.as_raw_fd(),
            BOOTSTRAP_ACK.as_ptr().cast(),
            BOOTSTRAP_ACK.len(),
            libc::MSG_NOSIGNAL,
        )
    };
    if sent < 0 {
        return Err(std::io::Error::last_os_error()).context("cannot send runtime proof ACK1");
    }
    if sent != BOOTSTRAP_ACK.len() as isize {
        bail!("runtime proof ACK1 packet was not sent completely");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn complete_bootstrap_ack(file: &File) -> Result<()> {
    if let Err(error) = send_bootstrap_ack(file) {
        crate::auth::erase_auth_authority()?;
        crate::signer::erase_signer_authority()?;
        return Err(error).context("cannot acknowledge protected runtime bootstrap");
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_bootstrap_socket(_file: &File) -> Result<()> {
    bail!("protected runtime proof requires Linux")
}

#[cfg(not(target_os = "linux"))]
fn read_bootstrap_frame(_file: &File) -> Result<(String, Zeroizing<[u8; SEED_BYTES]>)> {
    bail!("protected runtime proof requires Linux")
}

#[cfg(not(target_os = "linux"))]
fn send_bootstrap_ack(_file: &File) -> Result<()> {
    bail!("protected runtime proof requires Linux")
}

#[cfg(not(target_os = "linux"))]
fn complete_bootstrap_ack(_file: &File) -> Result<()> {
    bail!("protected runtime proof requires Linux")
}

pub(crate) fn current_artifact_sha256() -> Result<String> {
    let executable = std::env::current_exe().context("cannot locate runtime executable")?;
    let mut file = File::open(executable).context("cannot open runtime executable")?;
    let mut hash = sha2::Sha256::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        sha2::Digest::update(&mut hash, &buffer[..read]);
    }
    use sha2::Digest as _;
    let digest = hash.finalize();
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    Ok(output)
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;

#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
