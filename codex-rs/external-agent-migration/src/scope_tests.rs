use super::MigrationScope;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use std::fs;
use tempfile::TempDir;

#[test]
fn missing_cwd_selects_home_scope() {
    assert_eq!(
        MigrationScope::from_cwd(/*cwd*/ None).expect("resolve scope"),
        Some(MigrationScope::Home)
    );
}

#[test]
fn nested_cwd_selects_repository_root() {
    let root = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir(root.path().join(".git")).expect("create git directory");
    let nested = root.path().join("src").join("nested");
    std::fs::create_dir_all(&nested).expect("create nested directory");

    let expected = AbsolutePathBuf::from_absolute_path(root.path())
        .and_then(|path| path.canonicalize())
        .map(AbsolutePathBuf::into_path_buf)
        .expect("canonicalize repository root");
    assert_eq!(
        MigrationScope::from_cwd(Some(&nested)).expect("resolve scope"),
        Some(MigrationScope::Repository { root: expected })
    );
}

#[test]
fn nonexistent_cwd_has_no_scope() {
    let root = tempfile::tempdir().expect("tempdir");

    assert_eq!(
        MigrationScope::from_cwd(Some(&root.path().join("missing"))).expect("resolve scope"),
        None
    );
}

#[cfg(unix)]
#[test]
fn detect_repo_canonicalizes_relative_cwd() {
    let current_dir = fs::canonicalize(std::env::current_dir().expect("read current directory"))
        .expect("canonicalize current directory");
    let root = tempfile::Builder::new()
        .prefix("external-agent-relative-cwd-")
        .tempdir_in(&current_dir)
        .expect("create relative-cwd tempdir");
    let repo_root = root.path().join("repo");
    let nested = repo_root.join("nested");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(&nested).expect("create nested dir");
    let relative_nested = nested
        .strip_prefix(&current_dir)
        .expect("derive relative nested cwd");

    assert_eq!(
        MigrationScope::from_cwd(Some(relative_nested)).expect("resolve scope"),
        Some(MigrationScope::Repository {
            root: fs::canonicalize(repo_root).expect("canonicalize repository root"),
        })
    );
}

#[cfg(unix)]
#[test]
fn symlinked_cwd_selects_canonical_repository_root() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let nested = repo_root.join("nested");
    let linked_cwd = root.path().join("linked-cwd");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(&nested).expect("create nested dir");
    std::os::unix::fs::symlink(&nested, &linked_cwd).expect("create cwd symlink");

    assert_eq!(
        MigrationScope::from_cwd(Some(&linked_cwd)).expect("resolve scope"),
        Some(MigrationScope::Repository {
            root: fs::canonicalize(repo_root).expect("canonicalize repository root"),
        })
    );
}

#[cfg(windows)]
#[test]
fn detect_repo_canonicalizes_without_windows_verbatim_prefix() {
    let root = TempDir::new().expect("create tempdir");
    let repo_root = root.path().join("repo");
    let nested = repo_root.join("nested");
    fs::create_dir_all(repo_root.join(".git")).expect("create git dir");
    fs::create_dir_all(&nested).expect("create nested dir");

    let expected = AbsolutePathBuf::from_absolute_path(&repo_root)
        .and_then(|path| path.canonicalize())
        .map(AbsolutePathBuf::into_path_buf)
        .expect("canonicalize repository root");
    let Some(MigrationScope::Repository { root: actual }) =
        MigrationScope::from_cwd(Some(&nested)).expect("resolve scope")
    else {
        panic!("expected repository scope");
    };

    assert_eq!(actual, expected);
    assert!(!actual.to_string_lossy().starts_with(r"\\?\"));
}
