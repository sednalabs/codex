#[cfg(unix)]
use codex_utils_absolute_path::AbsolutePathBuf;
#[cfg(unix)]
use pretty_assertions::assert_eq;
#[cfg(unix)]
use std::io;
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use tempfile::tempdir;

#[cfg(unix)]
struct CurrentDirGuard {
    previous: PathBuf,
}

#[cfg(unix)]
impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
    }
}

#[cfg(unix)]
#[test]
fn absolute_paths_still_resolve_when_current_dir_is_missing() -> io::Result<()> {
    let target_root = tempdir()?;
    let target_dir = target_root.path().join("target");
    std::fs::create_dir(&target_dir)?;

    let cwd_root = tempdir()?;
    let cwd_dir = cwd_root.path().join("cwd");
    std::fs::create_dir(&cwd_dir)?;

    let _guard = CurrentDirGuard {
        previous: std::env::current_dir()?,
    };
    std::env::set_current_dir(&cwd_dir)?;
    std::fs::remove_dir(&cwd_dir)?;

    assert_eq!(
        std::env::current_dir().unwrap_err().kind(),
        io::ErrorKind::NotFound
    );

    let target_path = target_dir.join("..").join("target");
    let resolved = AbsolutePathBuf::from_absolute_path(&target_path)?;
    assert_eq!(resolved.as_path(), target_dir.as_path());
    Ok(())
}
