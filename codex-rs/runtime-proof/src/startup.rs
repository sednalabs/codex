use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use std::fs::File;
use std::io::Read;

const AUTH_DESCRIPTOR_ENV: &str = "OPS_RUNTIME_AUTH_FD";
const BROKER_DESCRIPTOR_ENV: &str = "OPS_RUNTIME_BROKER_FD";

pub fn initialize_from_environment() -> Result<()> {
    let auth_configured = std::env::var_os(AUTH_DESCRIPTOR_ENV).is_some();
    let broker_configured = std::env::var_os(BROKER_DESCRIPTOR_ENV).is_some();
    if !auth_configured && !broker_configured {
        return crate::signer::install_unconfigured();
    }
    if !auth_configured || !broker_configured {
        bail!("protected runtime auth and broker descriptors must be configured together");
    }
    #[cfg(target_os = "linux")]
    {
        verify_inherited_protection()?;
        set_non_dumpable()?;
        verify_runtime_protection()?;
        crate::signer::install_unconfigured()?;
        let auth_socket = match crate::auth::initialize_from_environment() {
            Ok(socket) => socket,
            Err(error) => {
                crate::signer::erase_signer_authority()?;
                return Err(error).context("initialize protected runtime authentication");
            }
        };
        if let Err(error) = crate::mcp_broker::install_child_socket_from_environment()
            .and_then(|()| verify_runtime_protection())
            .and_then(|()| crate::auth::send_auth_ack(&auth_socket))
        {
            crate::auth::erase_auth_authority()?;
            crate::signer::erase_signer_authority()?;
            return Err(error);
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        bail!("protected runtime proof requires Linux")
    }
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
mod tests {
    use super::*;

    #[test]
    fn current_artifact_digest_is_lowercase_sha256() {
        let digest = current_artifact_sha256().expect("current artifact digest");
        assert_eq!(digest.len(), 64);
        assert!(
            digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }
}
