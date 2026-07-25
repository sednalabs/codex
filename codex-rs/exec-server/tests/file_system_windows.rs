#![cfg(windows)]
#![allow(clippy::expect_used)]

mod common;

#[path = "file_system/shared.rs"]
mod shared;
#[path = "file_system/support.rs"]
mod support;

use std::path::Path;
use std::process::Command;

use anyhow::Result;
use codex_exec_server::ExecServerRuntimePaths;
use codex_exec_server::ExecutorFileSystem;
use codex_exec_server::FileSystemSandboxContext;
use codex_exec_server::LocalFileSystem;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_protocol::protocol::SandboxPolicy;
use codex_utils_path_uri::PathUri;
use test_case::test_case;

use crate::support::FileSystemImplementation;
use crate::support::create_file_system_context;

struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &std::ffi::OsStr) -> Self {
        let original = std::env::var_os(key);
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe {
            match &self.original {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

fn create_directory_junction(target: &Path, alias: &Path) -> Result<()> {
    let output = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(alias)
        .arg(target)
        .output()?;
    if !output.status.success() {
        anyhow::bail!(
            "mklink /J failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout).trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_canonicalize_resolves_directory_junction(
    implementation: FileSystemImplementation,
) -> Result<()> {
    shared::assert_canonicalize_resolves_directory_alias(implementation, create_directory_junction)
        .await
}

#[test_case(FileSystemImplementation::Local ; "local")]
#[test_case(FileSystemImplementation::Remote ; "remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_sandboxed_canonicalize_resolves_directory_junction(
    implementation: FileSystemImplementation,
) -> Result<()> {
    shared::assert_sandboxed_canonicalize_resolves_directory_alias(
        implementation,
        create_directory_junction,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_system_remote_fs_helper_respects_windows_sandbox_write_policy() -> Result<()> {
    let context = create_file_system_context(FileSystemImplementation::Remote).await?;
    let file_system = context.file_system;
    let tmp = tempfile::TempDir::new()?;
    let readonly_dir = tmp.path().join("readonly");
    std::fs::create_dir_all(&readonly_dir)?;

    let mut sandbox = read_only_sandbox_for_cwd(readonly_dir.clone())?;
    sandbox.windows_sandbox_level = WindowsSandboxLevel::RestrictedToken;
    // The gnullvm test binary is re-entered as the helper and needs a desktop
    // whose DACL explicitly admits the restricted logon SID during DLL startup.
    sandbox.windows_sandbox_private_desktop = true;

    let readable_file = readonly_dir.join("readable.txt");
    std::fs::write(&readable_file, b"readable")?;
    let read_result = file_system
        .read_file(
            &PathUri::from_host_native_path(&readable_file)?,
            Some(&sandbox),
        )
        .await;
    // Some local Windows hosts cannot create restricted tokens, and the Bazel
    // gnullvm test binary cannot always initialize when re-entered under the
    // downstream write-restricted token. Either exact startup error still
    // proves the helper traversed the sandbox wrapper; MSVC and other targets
    // continue through the read and denied-write assertions below.
    if is_unsupported_restricted_token_host(&read_result) {
        return Ok(());
    }
    assert_eq!(read_result?, b"readable");

    let blocked_file = readonly_dir.join("blocked.txt");
    let error = file_system
        .write_file(
            &PathUri::from_host_native_path(&blocked_file)?,
            b"blocked".to_vec(),
            Some(&sandbox),
        )
        .await
        .expect_err("write outside the sandbox should fail");
    assert!(
        !blocked_file.exists(),
        "sandboxed fs helper must not create blocked file after error: {error}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial(codex_home)]
async fn file_system_local_fs_helper_allows_windows_workspace_root_write() -> Result<()> {
    let codex_home = tempfile::TempDir::new()?;
    let _codex_home_guard = EnvVarGuard::set("CODEX_HOME", codex_home.path().as_os_str());
    let runtime_paths = ExecServerRuntimePaths::new(
        codex_utils_cargo_bin::cargo_bin("codex")?,
        /*codex_linux_sandbox_exe*/ None,
    )?;
    let file_system = LocalFileSystem::with_runtime_paths(runtime_paths);
    let workspace = tempfile::TempDir::new()?;
    let workspace_path = std::fs::canonicalize(workspace.path())?;
    let workspace_uri = PathUri::from_host_native_path(&workspace_path)?;
    let target_path = workspace_path.join("allowed.txt");
    let target_uri = PathUri::from_host_native_path(&target_path)?;
    let permissions = PermissionProfile::workspace_write_with(
        &[],
        NetworkSandboxPolicy::Restricted,
        /*exclude_tmpdir_env_var*/ true,
        /*exclude_slash_tmp*/ true,
    );
    let mut sandbox =
        FileSystemSandboxContext::from_permission_profile_with_cwd(permissions, workspace_uri);
    sandbox.windows_sandbox_level = WindowsSandboxLevel::RestrictedToken;
    sandbox.windows_sandbox_private_desktop = true;

    let write_result = file_system
        .write_file(&target_uri, b"allowed".to_vec(), Some(&sandbox))
        .await;
    if is_unsupported_restricted_token_host(&write_result) {
        eprintln!("skipping release-shaped assertion: {write_result:?}");
        return Ok(());
    }
    write_result?;

    assert_eq!(std::fs::read(target_path)?, b"allowed");
    Ok(())
}

fn read_only_sandbox_for_cwd(cwd: std::path::PathBuf) -> Result<FileSystemSandboxContext> {
    Ok(FileSystemSandboxContext::from_legacy_sandbox_policy(
        SandboxPolicy::new_read_only_policy(),
        PathUri::from_host_native_path(cwd)?,
    )?)
}

fn is_unsupported_restricted_token_host<T>(result: &std::io::Result<T>) -> bool {
    result.as_ref().err().is_some_and(|err| {
        let message = err.to_string();
        message.contains("windows sandbox failed: CreateRestrictedToken failed: 87")
            || (cfg!(target_env = "gnu")
                && message.contains("fs sandbox helper failed with status exit code: 0xc0000142"))
    })
}
