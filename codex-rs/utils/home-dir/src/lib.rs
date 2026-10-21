use codex_utils_absolute_path::AbsolutePathBuf;
use dirs::home_dir;
#[cfg(unix)]
use std::ffi::CStr;
#[cfg(unix)]
use std::mem::MaybeUninit;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

/// Returns the path to the Codex configuration directory, which can be
/// specified by the `CODEX_HOME` environment variable. If not set, defaults to
/// `~/.codex`.
///
/// - If `CODEX_HOME` is set, the value must exist and be a directory. The
///   value will be canonicalized and this function will Err otherwise.
/// - If `CODEX_HOME` is not set, this function does not verify that the
///   directory exists.
pub fn find_codex_home() -> std::io::Result<AbsolutePathBuf> {
    let codex_home_env = std::env::var("CODEX_HOME")
        .ok()
        .filter(|val| !val.is_empty());
    find_codex_home_from_env(codex_home_env.as_deref())
}

/// Returns the stable default Codex home without consulting `CODEX_HOME`.
///
/// Credential stores shared by an OS user must use the same lock location even
/// when individual Codex processes select different configuration homes.
pub fn find_default_codex_home() -> std::io::Result<AbsolutePathBuf> {
    let mut path = home_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not find home directory",
        )
    })?;
    path.push(".codex");
    AbsolutePathBuf::from_absolute_path_checked(path)
}

/// Returns the effective OS user's default Codex home from the passwd database.
///
/// This deliberately ignores `HOME` and `CODEX_HOME`. It is for credentials
/// shared by an OS user across Codex configuration homes, where every caller
/// must rendezvous on the same refresh lock.
#[cfg(unix)]
pub fn find_default_codex_home_for_current_user() -> std::io::Result<AbsolutePathBuf> {
    let uid = unsafe { libc::geteuid() };
    let mut buffer = vec![0_u8; 64 * 1024];
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
    if status != 0 {
        return Err(std::io::Error::from_raw_os_error(status));
    }
    if entry.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Could not resolve effective user's passwd entry",
        ));
    }

    let passwd = unsafe { passwd.assume_init_ref() };
    if passwd.pw_uid != uid || passwd.pw_dir.is_null() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Effective user's passwd entry has no home directory",
        ));
    }
    let home = unsafe { CStr::from_ptr(passwd.pw_dir) };
    let home = PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes()));
    if !home.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Effective user's passwd home is not absolute",
        ));
    }
    let path = home.join(".codex");
    AbsolutePathBuf::from_absolute_path_checked(path)
}

/// Uses the platform's configured home on systems without a Unix passwd DB.
#[cfg(not(unix))]
pub fn find_default_codex_home_for_current_user() -> std::io::Result<AbsolutePathBuf> {
    find_default_codex_home()
}

fn find_codex_home_from_env(codex_home_env: Option<&str>) -> std::io::Result<AbsolutePathBuf> {
    // Honor the `CODEX_HOME` environment variable when it is set to allow users
    // (and tests) to override the default location.
    match codex_home_env {
        Some(val) => {
            let path = PathBuf::from(val);
            let metadata = std::fs::metadata(&path).map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("CODEX_HOME points to {val:?}, but that path does not exist"),
                ),
                _ => std::io::Error::new(
                    err.kind(),
                    format!("failed to read CODEX_HOME {val:?}: {err}"),
                ),
            })?;

            if !metadata.is_dir() {
                Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("CODEX_HOME points to {val:?}, but that path is not a directory"),
                ))
            } else {
                let canonical = path.canonicalize().map_err(|err| {
                    std::io::Error::new(
                        err.kind(),
                        format!("failed to canonicalize CODEX_HOME {val:?}: {err}"),
                    )
                })?;
                AbsolutePathBuf::from_absolute_path_checked(canonical)
            }
        }
        None => find_default_codex_home(),
    }
}

#[cfg(test)]
mod tests {
    use super::find_codex_home_from_env;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use dirs::home_dir;
    use pretty_assertions::assert_eq;
    #[cfg(unix)]
    use std::ffi::CStr;
    use std::fs;
    use std::io::ErrorKind;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn find_codex_home_env_missing_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let missing = temp_home.path().join("missing-codex-home");
        let missing_str = missing
            .to_str()
            .expect("missing codex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(missing_str)).expect_err("missing CODEX_HOME");
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert!(
            err.to_string().contains("CODEX_HOME"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_file_path_is_fatal() {
        let temp_home = TempDir::new().expect("temp home");
        let file_path = temp_home.path().join("codex-home.txt");
        fs::write(&file_path, "not a directory").expect("write temp file");
        let file_str = file_path
            .to_str()
            .expect("file codex home path should be valid utf-8");

        let err = find_codex_home_from_env(Some(file_str)).expect_err("file CODEX_HOME");
        assert_eq!(err.kind(), ErrorKind::InvalidInput);
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn find_codex_home_env_valid_directory_canonicalizes() {
        let temp_home = TempDir::new().expect("temp home");
        let temp_str = temp_home
            .path()
            .to_str()
            .expect("temp codex home path should be valid utf-8");

        let resolved = find_codex_home_from_env(Some(temp_str)).expect("valid CODEX_HOME");
        let expected = temp_home
            .path()
            .canonicalize()
            .expect("canonicalize temp home");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn find_codex_home_without_env_uses_default_home_dir() {
        let resolved =
            find_codex_home_from_env(/*codex_home_env*/ None).expect("default CODEX_HOME");
        let mut expected = home_dir().expect("home dir");
        expected.push(".codex");
        let expected = AbsolutePathBuf::from_absolute_path(expected).expect("absolute home");
        assert_eq!(resolved, expected);
    }

    #[cfg(unix)]
    #[test]
    fn default_codex_home_for_current_user_uses_passwd_home() {
        let resolved =
            super::find_default_codex_home_for_current_user().expect("passwd-derived home");
        let uid = unsafe { libc::geteuid() };
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut passwd = std::mem::MaybeUninit::<libc::passwd>::uninit();
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
        assert_eq!(status, 0);
        assert!(!entry.is_null());
        let passwd = unsafe { passwd.assume_init_ref() };
        let home = unsafe { CStr::from_ptr(passwd.pw_dir) };
        let home = PathBuf::from(std::ffi::OsStr::from_bytes(home.to_bytes()));
        assert_eq!(resolved.as_path(), &home.join(".codex"));
    }

    #[cfg(unix)]
    #[test]
    fn default_codex_home_for_current_user_ignores_environment_overrides() {
        let home_override = TempDir::new().expect("HOME override");
        let codex_home_override = TempDir::new().expect("CODEX_HOME override");
        let output = std::process::Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--exact",
                "tests::default_codex_home_for_current_user_uses_passwd_home",
                "--nocapture",
            ])
            .env("HOME", home_override.path())
            .env("CODEX_HOME", codex_home_override.path())
            .output()
            .expect("run child test with conflicting home overrides");
        assert!(
            output.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
