use std::collections::HashMap;

use codex_exec_server_protocol::JSONRPCErrorError;
use codex_protocol::models::PermissionProfile;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::permissions::NetworkSandboxPolicy;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxDirectSpawnTransformRequest;
use codex_sandboxing::SandboxExecRequest;
use codex_sandboxing::SandboxManager;
use codex_sandboxing::SandboxTransformRequest;
use codex_sandboxing::SandboxablePreference;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_absolute_path::canonicalize_preserving_symlinks;
use codex_utils_path_uri::PathUri;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::ExecServerRuntimePaths;
use crate::FileSystemSandboxContext;
use crate::fs_helper::CODEX_FS_HELPER_ARG1;
use crate::fs_helper::FsHelperPayload;
use crate::fs_helper::FsHelperRequest;
use crate::fs_helper::FsHelperResponse;
use crate::local_file_system::current_sandbox_cwd;
use crate::rpc::internal_error;
use crate::rpc::invalid_request;

const FS_HELPER_ENV_ALLOWLIST: &[&str] = &["PATH", "TMPDIR", "TMP", "TEMP"];
#[cfg(debug_assertions)]
const FS_HELPER_BAZEL_BWRAP_ENV_ALLOWLIST: &[&str] = &[
    "CARGO_BIN_EXE_bwrap",
    "RUNFILES_DIR",
    "RUNFILES_MANIFEST_FILE",
    "RUNFILES_MANIFEST_ONLY",
    "TEST_SRCDIR",
    "TEST_WORKSPACE",
];

#[derive(Debug, PartialEq, Eq)]
struct SandboxCwd {
    uri: PathUri,
    native: AbsolutePathBuf,
}

#[derive(Clone, Debug)]
pub(crate) struct FileSystemSandboxRunner {
    runtime_paths: ExecServerRuntimePaths,
    helper_env: HashMap<String, String>,
}

impl FileSystemSandboxRunner {
    pub(crate) fn new(runtime_paths: ExecServerRuntimePaths) -> Self {
        Self {
            runtime_paths,
            helper_env: helper_env(),
        }
    }

    pub(crate) async fn run(
        &self,
        sandbox: &FileSystemSandboxContext,
        request: FsHelperRequest,
    ) -> Result<FsHelperPayload, JSONRPCErrorError> {
        let cwd = sandbox_cwd(sandbox)?;
        let helper = self.helper_exe_for_launch()?;
        let native_workspace_roots = sandbox
            .workspace_roots
            .iter()
            .map(native_workspace_root)
            .collect::<Result<Vec<_>, _>>()?;
        let workspace_roots = native_workspace_roots.as_slice();
        let native_permissions: PermissionProfile =
            sandbox.permissions.clone().try_into().map_err(|err| {
                invalid_request(format!("invalid sandbox permission path URI: {err}"))
            })?;
        let native_permissions =
            native_permissions.materialize_project_roots_with_workspace_roots(workspace_roots);
        let mut file_system_policy = native_permissions.file_system_sandbox_policy();
        let helper_read_roots = if sandbox.use_legacy_landlock {
            Vec::new()
        } else {
            helper_read_roots(&helper, self.runtime_paths.codex_linux_sandbox_exe.as_ref())
        };
        add_helper_runtime_permissions(
            &mut file_system_policy,
            &helper_read_roots,
            cwd.native.as_path(),
        );
        normalize_file_system_policy_root_aliases(&mut file_system_policy);
        let network_policy = NetworkSandboxPolicy::Restricted;
        let permission_profile = PermissionProfile::from_runtime_permissions_with_enforcement(
            native_permissions.enforcement(),
            &file_system_policy,
            network_policy,
        );
        let command = self.sandbox_exec_request(
            &permission_profile,
            &cwd,
            workspace_roots,
            sandbox,
            &helper,
        )?;
        let request_json = serde_json::to_vec(&request).map_err(json_error)?;
        run_command(command, request_json, workspace_roots).await
    }

    fn helper_exe_for_launch(&self) -> Result<AbsolutePathBuf, JSONRPCErrorError> {
        #[cfg(target_os = "windows")]
        {
            let codex_home = codex_utils_home_dir::find_codex_home().map_err(|err| {
                internal_error(format!(
                    "windows fs sandbox helper failed to resolve CODEX_HOME: {err}"
                ))
            })?;
            let helper = codex_windows_sandbox::resolve_exe_for_launch(
                self.runtime_paths.codex_self_exe.as_path(),
                codex_home.as_path(),
            );
            AbsolutePathBuf::from_absolute_path(helper.as_path()).map_err(|err| {
                internal_error(format!(
                    "windows fs sandbox helper path is not absolute: {err}"
                ))
            })
        }

        #[cfg(not(target_os = "windows"))]
        {
            Ok(self.runtime_paths.codex_self_exe.clone())
        }
    }

    fn sandbox_exec_request(
        &self,
        permission_profile: &PermissionProfile,
        cwd: &SandboxCwd,
        workspace_roots: &[AbsolutePathBuf],
        sandbox_context: &FileSystemSandboxContext,
        helper: &AbsolutePathBuf,
    ) -> Result<SandboxExecRequest, JSONRPCErrorError> {
        let sandbox_manager = SandboxManager::new();
        let (file_system_policy, network_policy) = permission_profile.to_runtime_permissions();
        let sandbox = sandbox_manager.select_initial(
            &file_system_policy,
            network_policy,
            SandboxablePreference::Auto,
            sandbox_context.windows_sandbox_level,
            /*has_managed_network_requirements*/ false,
        );
        let command = SandboxCommand {
            program: helper.as_path().as_os_str().to_owned(),
            args: vec![CODEX_FS_HELPER_ARG1.to_string()],
            cwd: cwd.uri.clone(),
            env: self.helper_env.clone(),
            managed_network: None,
            additional_permissions: None,
        };
        sandbox_manager
            .transform_for_direct_spawn(SandboxDirectSpawnTransformRequest {
                workspace_roots,
                windows_sandbox_proxy_settings_mode:
                    codex_sandboxing::WindowsSandboxProxySettingsMode::Preserve,
                transform: SandboxTransformRequest {
                    command,
                    permissions: permission_profile,
                    sandbox,
                    enforce_managed_network: false,
                    environment_id: None,
                    network: None,
                    sandbox_policy_cwd: &cwd.uri,
                    codex_linux_sandbox_exe: self.runtime_paths.codex_linux_sandbox_exe.as_deref(),
                    use_legacy_landlock: sandbox_context.use_legacy_landlock,
                    windows_sandbox_level: sandbox_context.windows_sandbox_level,
                    windows_sandbox_private_desktop: sandbox_context
                        .windows_sandbox_private_desktop,
                },
            })
            .map_err(|err| invalid_request(format!("failed to prepare fs sandbox: {err}")))
    }
}

fn sandbox_cwd(sandbox: &FileSystemSandboxContext) -> Result<SandboxCwd, JSONRPCErrorError> {
    if let Some(uri) = &sandbox.cwd {
        return Ok(SandboxCwd {
            native: native_sandbox_cwd(uri)?,
            uri: uri.clone(),
        });
    }

    if sandbox.has_cwd_dependent_permissions() {
        return Err(invalid_request(
            "file system sandbox context with dynamic permissions requires cwd".to_string(),
        ));
    }

    let native = AbsolutePathBuf::from_absolute_path(current_sandbox_cwd().map_err(io_error)?)
        .map_err(|err| invalid_request(format!("current directory is not absolute: {err}")))?;
    let uri = PathUri::from_abs_path(&native);
    Ok(SandboxCwd { uri, native })
}

fn native_sandbox_cwd(cwd: &PathUri) -> Result<AbsolutePathBuf, JSONRPCErrorError> {
    cwd.to_abs_path()
        .map_err(|err| invalid_request(err.to_string()))
}

fn native_workspace_root(root: &PathUri) -> Result<AbsolutePathBuf, JSONRPCErrorError> {
    root.to_abs_path().map_err(|err| {
        invalid_request(format!(
            "file system sandbox workspace root is not native to this exec-server host: {err}"
        ))
    })
}

fn helper_read_roots(
    codex_self_exe: &AbsolutePathBuf,
    codex_linux_sandbox_exe: Option<&AbsolutePathBuf>,
) -> Vec<AbsolutePathBuf> {
    let mut roots = Vec::new();
    for path in std::iter::once(codex_self_exe.as_path())
        .chain(codex_linux_sandbox_exe.map(AbsolutePathBuf::as_path))
    {
        if let Some(parent) = path.parent()
            && let Ok(root) = AbsolutePathBuf::from_absolute_path(parent)
            && !roots.contains(&root)
        {
            roots.push(root);
        }
    }
    roots
}

fn add_helper_runtime_permissions(
    file_system_policy: &mut FileSystemSandboxPolicy,
    helper_read_roots: &[AbsolutePathBuf],
    cwd: &std::path::Path,
) {
    if !file_system_policy.has_full_disk_read_access() {
        let minimal_read_entry = FileSystemSandboxEntry::new(
            FileSystemPath::Special {
                value: FileSystemSpecialPath::Minimal,
            },
            FileSystemAccessMode::Read,
        );
        if !file_system_policy.entries.contains(&minimal_read_entry) {
            file_system_policy.entries.push(minimal_read_entry);
        }
    }

    for helper_read_root in helper_read_roots {
        if file_system_policy.can_read_path_with_cwd(helper_read_root.as_path(), cwd) {
            continue;
        }

        file_system_policy.entries.push(FileSystemSandboxEntry::new(
            FileSystemPath::Path {
                path: helper_read_root.clone(),
            },
            FileSystemAccessMode::Read,
        ));
    }
}

fn normalize_file_system_policy_root_aliases(file_system_policy: &mut FileSystemSandboxPolicy) {
    for entry in &mut file_system_policy.entries {
        if let FileSystemPath::Path { path } = &mut entry.path {
            *path = normalize_top_level_alias(path.clone());
        }
    }
}

fn normalize_top_level_alias(path: AbsolutePathBuf) -> AbsolutePathBuf {
    let raw_path = path.to_path_buf();
    for ancestor in raw_path.ancestors() {
        if std::fs::symlink_metadata(ancestor).is_err() {
            continue;
        }
        let Ok(normalized_ancestor) = canonicalize_preserving_symlinks(ancestor) else {
            continue;
        };
        if normalized_ancestor == ancestor {
            continue;
        }
        let Ok(suffix) = raw_path.strip_prefix(ancestor) else {
            continue;
        };
        if let Ok(normalized_path) =
            AbsolutePathBuf::from_absolute_path(normalized_ancestor.join(suffix))
        {
            return normalized_path;
        }
    }
    path
}

fn helper_env() -> HashMap<String, String> {
    helper_env_from_vars(std::env::vars_os())
}

fn helper_env_from_vars(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> HashMap<String, String> {
    vars.into_iter()
        .filter_map(|(key, value)| {
            let key = key.to_string_lossy();
            helper_env_key_is_allowed(&key)
                .then(|| (key.into_owned(), value.to_string_lossy().into_owned()))
        })
        .collect()
}

fn helper_env_key_is_allowed(key: &str) -> bool {
    FS_HELPER_ENV_ALLOWLIST.contains(&key)
        // CoreFoundation consults this before falling back to user lookup during helper startup.
        || (cfg!(target_os = "macos") && key == "__CF_USER_TEXT_ENCODING")
        || bazel_bwrap_env_key_is_allowed(key)
        || (cfg!(windows)
            && (key.eq_ignore_ascii_case("PATH") || key.eq_ignore_ascii_case("SYSTEMROOT")))
}

#[cfg(debug_assertions)]
fn bazel_bwrap_env_key_is_allowed(key: &str) -> bool {
    option_env!("BAZEL_PACKAGE").is_some() && FS_HELPER_BAZEL_BWRAP_ENV_ALLOWLIST.contains(&key)
}

#[cfg(not(debug_assertions))]
fn bazel_bwrap_env_key_is_allowed(_key: &str) -> bool {
    false
}

async fn run_command(
    command: SandboxExecRequest,
    request_json: Vec<u8>,
    workspace_roots: &[AbsolutePathBuf],
) -> Result<FsHelperPayload, JSONRPCErrorError> {
    let output = run_helper_command(command, request_json, workspace_roots).await?;
    if !output.success {
        return Err(internal_error(format!(
            "fs sandbox helper failed with status {status}: {stderr}",
            status = output.status,
            stderr = String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let response: FsHelperResponse = serde_json::from_slice(&output.stdout).map_err(json_error)?;
    match response {
        FsHelperResponse::Ok(payload) => Ok(payload),
        FsHelperResponse::Error(error) => Err(error),
    }
}

struct FsHelperCommandOutput {
    success: bool,
    status: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

async fn run_helper_command(
    command: SandboxExecRequest,
    request_json: Vec<u8>,
    _workspace_roots: &[AbsolutePathBuf],
) -> Result<FsHelperCommandOutput, JSONRPCErrorError> {
    #[cfg(target_os = "windows")]
    if command.sandbox == codex_sandboxing::SandboxType::WindowsRestrictedToken {
        return run_windows_sandbox_command(command, request_json, _workspace_roots).await;
    }

    run_child_command(command, request_json).await
}

async fn run_child_command(
    command: SandboxExecRequest,
    request_json: Vec<u8>,
) -> Result<FsHelperCommandOutput, JSONRPCErrorError> {
    let mut child = spawn_child_command(command)?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| internal_error("failed to open fs sandbox helper stdin".to_string()))?;
    stdin.write_all(&request_json).await.map_err(io_error)?;
    stdin.shutdown().await.map_err(io_error)?;
    drop(stdin);

    let output = child.wait_with_output().await.map_err(io_error)?;
    Ok(FsHelperCommandOutput {
        success: output.status.success(),
        status: output.status.to_string(),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn spawn_child_command(
    SandboxExecRequest {
        command: argv,
        cwd,
        env,
        arg0,
        ..
    }: SandboxExecRequest,
) -> Result<tokio::process::Child, JSONRPCErrorError> {
    let Some((program, args)) = argv.split_first() else {
        return Err(invalid_request("fs sandbox command was empty".to_string()));
    };
    let mut command = Command::new(program);
    #[cfg(unix)]
    if let Some(arg0) = arg0 {
        command.arg0(arg0);
    }
    #[cfg(not(unix))]
    let _ = arg0;
    command.args(args);
    // TODO(anp): Keep PathUri through the filesystem helper launch boundary.
    let cwd = cwd.to_abs_path().map_err(io_error)?;
    command.current_dir(cwd.as_path());
    command.env_clear();
    command.envs(env);
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);
    command.spawn().map_err(io_error)
}

#[cfg(target_os = "windows")]
#[derive(Debug)]
struct WindowsFsHelperRequestFile {
    path: std::path::PathBuf,
}

#[cfg(target_os = "windows")]
impl WindowsFsHelperRequestFile {
    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn cleanup(mut self) -> std::io::Result<()> {
        let result = remove_windows_fs_helper_request_file(&self.path);
        if result.is_ok() {
            self.path.clear();
        }
        result
    }
}

#[cfg(target_os = "windows")]
impl Drop for WindowsFsHelperRequestFile {
    fn drop(&mut self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        if let Err(err) = remove_windows_fs_helper_request_file(&self.path) {
            tracing::warn!(
                path = %self.path.display(),
                error = %err,
                "failed to clean up windows fs sandbox helper request file"
            );
        }
    }
}

#[cfg(target_os = "windows")]
fn remove_windows_fs_helper_request_file(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(target_os = "windows")]
fn write_windows_fs_helper_request_file(
    helper_program: &str,
    request_json: &[u8],
) -> Result<WindowsFsHelperRequestFile, JSONRPCErrorError> {
    write_windows_fs_helper_request_file_with(helper_program, |file| {
        std::io::Write::write_all(file, request_json)
    })
}

#[cfg(target_os = "windows")]
fn write_windows_fs_helper_request_file_with(
    helper_program: &str,
    write_request: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<WindowsFsHelperRequestFile, JSONRPCErrorError> {
    let helper_dir = std::path::Path::new(helper_program)
        .parent()
        .ok_or_else(|| {
            invalid_request("windows fs sandbox helper path has no parent".to_string())
        })?;
    let path = helper_dir.join(format!(
        ".codex-fs-helper-request-{}.json",
        uuid::Uuid::new_v4()
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|err| {
            internal_error(format!(
                "failed to create windows fs sandbox helper request file {}: {err}",
                path.display()
            ))
        })?;
    let request_file = WindowsFsHelperRequestFile { path };
    if let Err(write_err) = write_request(&mut file) {
        drop(file);
        let path = request_file.path().display().to_string();
        let cleanup_result = request_file.cleanup();
        return Err(internal_error(match cleanup_result {
            Ok(()) => format!(
                "failed to write windows fs sandbox helper request file {path}: {write_err}"
            ),
            Err(cleanup_err) => format!(
                "failed to write windows fs sandbox helper request file {path}: {write_err}; \
                 cleanup also failed: {cleanup_err}"
            ),
        }));
    }
    drop(file);
    Ok(request_file)
}

#[cfg(target_os = "windows")]
async fn run_windows_sandbox_command(
    SandboxExecRequest {
        mut command,
        cwd,
        env,
        windows_sandbox_level,
        windows_sandbox_private_desktop,
        permission_profile,
        ..
    }: SandboxExecRequest,
    request_json: Vec<u8>,
    workspace_roots: &[AbsolutePathBuf],
) -> Result<FsHelperCommandOutput, JSONRPCErrorError> {
    if command.is_empty() {
        return Err(invalid_request("fs sandbox command was empty".to_string()));
    }
    let codex_home = codex_utils_home_dir::find_codex_home().map_err(|err| {
        internal_error(format!(
            "windows fs sandbox helper failed to resolve CODEX_HOME: {err}"
        ))
    })?;
    let request_file = write_windows_fs_helper_request_file(&command[0], &request_json)?;
    command.push(request_file.path().to_string_lossy().into_owned());
    let cwd = cwd.to_abs_path().map_err(io_error)?;
    let empty_paths: &[AbsolutePathBuf] = &[];
    let spawned = codex_windows_sandbox::spawn_windows_sandbox_session_for_level(
        codex_windows_sandbox::WindowsSandboxSessionRequest {
            permission_profile: &permission_profile,
            workspace_roots,
            codex_home: codex_home.as_path(),
            command,
            cwd: cwd.as_path(),
            env_map: env,
            windows_sandbox_level,
            proxy_enforced: false,
            network_proxy_restricting_sid: None,
            proxy_settings_mode: codex_windows_sandbox::WindowsSandboxProxySettingsMode::Preserve,
            timeout_ms: None,
            read_roots_override: None,
            read_roots_include_platform_defaults: false,
            write_roots_override: None,
            deny_read_paths_override: empty_paths,
            deny_write_paths_override: empty_paths,
            tty: false,
            stdin_open: false,
            use_private_desktop: windows_sandbox_private_desktop,
        },
    )
    .await;
    let codex_utils_pty::SpawnedProcess {
        session: _session,
        mut stdout_rx,
        mut stderr_rx,
        exit_rx,
    } = match spawned {
        Ok(spawned) => spawned,
        Err(err) => {
            return Err(internal_error(format!(
                "windows fs sandbox helper failed to run: {err}"
            )));
        }
    };
    let stdout_task = tokio::spawn(async move {
        let mut stdout = Vec::new();
        while let Some(chunk) = stdout_rx.recv().await {
            stdout.extend(chunk);
        }
        stdout
    });
    let stderr_task = tokio::spawn(async move {
        let mut stderr = Vec::new();
        while let Some(chunk) = stderr_rx.recv().await {
            stderr.extend(chunk);
        }
        stderr
    });
    let exit_code = exit_rx.await.unwrap_or(-1);
    request_file.cleanup().map_err(|err| {
        internal_error(format!(
            "failed to clean up windows fs sandbox helper request file: {err}"
        ))
    })?;
    let stdout = stdout_task
        .await
        .map_err(|err| internal_error(format!("windows fs helper stdout task failed: {err}")))?;
    let stderr = stderr_task
        .await
        .map_err(|err| internal_error(format!("windows fs helper stderr task failed: {err}")))?;

    Ok(FsHelperCommandOutput {
        success: exit_code == 0,
        status: format!("exit code {exit_code}"),
        stdout,
        stderr,
    })
}

fn io_error(err: std::io::Error) -> JSONRPCErrorError {
    internal_error(err.to_string())
}

fn json_error(err: serde_json::Error) -> JSONRPCErrorError {
    internal_error(format!(
        "failed to encode or decode fs sandbox helper message: {err}"
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;

    use codex_protocol::models::PermissionProfile;
    use codex_protocol::permissions::FileSystemAccessMode;
    use codex_protocol::permissions::FileSystemPath;
    use codex_protocol::permissions::FileSystemSandboxEntry;
    use codex_protocol::permissions::FileSystemSandboxPolicy;
    use codex_protocol::permissions::FileSystemSpecialPath;
    use codex_protocol::permissions::NetworkSandboxPolicy;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_path_uri::PathUri;
    use pretty_assertions::assert_eq;

    use crate::ExecServerRuntimePaths;

    use super::FileSystemSandboxRunner;
    use super::SandboxCwd;
    use super::add_helper_runtime_permissions;
    use super::helper_env;
    use super::helper_env_from_vars;
    use super::helper_env_key_is_allowed;
    use super::helper_read_roots;
    use super::sandbox_cwd;
    #[cfg(target_os = "windows")]
    use super::write_windows_fs_helper_request_file;
    #[cfg(target_os = "windows")]
    use super::write_windows_fs_helper_request_file_with;

    #[test]
    fn helper_permissions_enable_minimal_reads_for_restricted_profile() {
        let cwd = AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path())
            .expect("absolute cwd");
        let mut policy = restricted_policy(Vec::new());

        add_helper_runtime_permissions(&mut policy, /*helper_read_roots*/ &[], cwd.as_path());

        assert!(policy.include_platform_defaults());
    }

    #[test]
    fn helper_permissions_enable_minimal_reads_for_restricted_profile_with_writes() {
        let cwd = AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path())
            .expect("absolute cwd");
        let mut policy = restricted_policy(vec![path_entry(
            cwd.join("writable"),
            FileSystemAccessMode::Write,
        )]);

        add_helper_runtime_permissions(&mut policy, /*helper_read_roots*/ &[], cwd.as_path());

        assert!(policy.include_platform_defaults());
    }

    #[test]
    fn helper_permissions_preserve_existing_writes() {
        let codex_self_exe = std::env::current_exe().expect("current exe");
        let runtime_paths =
            ExecServerRuntimePaths::new(codex_self_exe, /*codex_linux_sandbox_exe*/ None)
                .expect("runtime paths");
        let cwd = AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path())
            .expect("absolute cwd");
        let writable = cwd.join("writable");
        let mut policy = restricted_policy(vec![path_entry(
            writable.clone(),
            FileSystemAccessMode::Write,
        )]);
        let readable = AbsolutePathBuf::from_absolute_path(
            runtime_paths
                .codex_self_exe
                .parent()
                .expect("current exe parent"),
        )
        .expect("absolute readable path");

        add_helper_runtime_permissions(
            &mut policy,
            &helper_read_roots(&runtime_paths.codex_self_exe, None),
            cwd.as_path(),
        );

        assert!(policy.can_read_path_with_cwd(readable.as_path(), cwd.as_path()));
        assert!(policy.can_write_path_with_cwd(writable.as_path(), cwd.as_path()));
    }

    #[test]
    fn helper_env_carries_only_allowlisted_runtime_vars() {
        let env = helper_env();

        let expected = std::env::vars_os()
            .filter_map(|(key, value)| {
                let key = key.to_string_lossy();
                helper_env_key_is_allowed(&key)
                    .then(|| (key.into_owned(), value.to_string_lossy().into_owned()))
            })
            .collect::<HashMap<_, _>>();

        assert_eq!(env, expected);
    }

    #[test]
    fn helper_env_preserves_path_for_system_bwrap_discovery_without_leaking_secrets() {
        let env = helper_env_from_vars(
            [
                ("PATH", "/usr/bin:/bin"),
                ("TMPDIR", "/tmp/codex"),
                ("TMP", "/tmp"),
                ("TEMP", "/tmp"),
                ("HOME", "/home/user"),
                ("OPENAI_API_KEY", "secret"),
                ("HTTPS_PROXY", "http://proxy.example"),
            ]
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );

        assert_eq!(
            env,
            HashMap::from([
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("TMPDIR".to_string(), "/tmp/codex".to_string()),
                ("TMP".to_string(), "/tmp".to_string()),
                ("TEMP".to_string(), "/tmp".to_string()),
            ])
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn helper_env_preserves_corefoundation_text_encoding() {
        let env = helper_env_from_vars(
            [
                ("__CF_USER_TEXT_ENCODING", "0x1F6:0x0:0x0"),
                ("HOME", "/Users/test"),
            ]
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );

        assert_eq!(
            env,
            HashMap::from([(
                "__CF_USER_TEXT_ENCODING".to_string(),
                "0x1F6:0x0:0x0".to_string(),
            )])
        );
    }

    #[cfg(windows)]
    #[test]
    fn helper_env_preserves_windows_runtime_keys_without_leaking_secrets() {
        let env = helper_env_from_vars(
            [
                ("Path", r"C:\Windows\System32"),
                ("SYSTEMROOT", r"C:\Windows"),
                ("PATH_INJECTION", "bad"),
                ("SYSTEMROOT_BACKUP", r"C:\Windows.old"),
                ("OPENAI_API_KEY", "secret"),
            ]
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );

        assert_eq!(
            env,
            HashMap::from([
                ("Path".to_string(), r"C:\Windows\System32".to_string()),
                ("SYSTEMROOT".to_string(), r"C:\Windows".to_string()),
            ])
        );
    }

    #[test]
    fn sandbox_exec_request_carries_helper_env() {
        let Some((path_key, path)) = std::env::vars_os().find(|(key, _)| {
            let key = key.to_string_lossy();
            key == "PATH" || (cfg!(windows) && key.eq_ignore_ascii_case("PATH"))
        }) else {
            return;
        };
        let path_key = path_key.to_string_lossy().into_owned();
        let path = path.to_string_lossy().into_owned();
        let codex_self_exe = std::env::current_exe().expect("current exe");
        let runtime_paths =
            ExecServerRuntimePaths::new(codex_self_exe.clone(), Some(codex_self_exe))
                .expect("runtime paths");
        let runner = FileSystemSandboxRunner::new(runtime_paths);
        let native_cwd = AbsolutePathBuf::current_dir().expect("cwd");
        let cwd = PathUri::from_abs_path(&native_cwd);
        let file_system_policy = restricted_policy(vec![path_entry(
            native_cwd.clone(),
            FileSystemAccessMode::Write,
        )]);
        let network_policy = NetworkSandboxPolicy::Restricted;
        let permission_profile =
            PermissionProfile::from_runtime_permissions(&file_system_policy, network_policy);
        let sandbox_context = sandbox_context_with_cwd(&file_system_policy, cwd.clone());
        let sandbox_cwd = SandboxCwd {
            uri: cwd,
            native: native_cwd,
        };

        let request = runner
            .sandbox_exec_request(
                &permission_profile,
                &sandbox_cwd,
                std::slice::from_ref(&sandbox_cwd.native),
                &sandbox_context,
                &runner.runtime_paths.codex_self_exe,
            )
            .expect("sandbox exec request");

        assert_eq!(request.env.get(&path_key), Some(&path));
    }

    #[test]
    fn sandbox_cwd_uses_context_cwd() {
        let native_cwd = AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path())
            .expect("absolute cwd");
        let cwd = PathUri::from_abs_path(&native_cwd);
        let policy = restricted_policy(vec![special_entry(
            FileSystemSpecialPath::project_roots(/*subpath*/ None),
            FileSystemAccessMode::Write,
        )]);
        let sandbox_context = sandbox_context_with_cwd(&policy, cwd.clone());

        assert_eq!(
            sandbox_cwd(&sandbox_context).expect("sandbox cwd"),
            SandboxCwd {
                uri: cwd,
                native: native_cwd
            }
        );
    }

    #[test]
    fn sandbox_cwd_rejects_non_native_context_cwd_without_fallback() {
        let cwd = non_native_cwd();
        let policy = restricted_policy(vec![special_entry(
            FileSystemSpecialPath::project_roots(/*subpath*/ None),
            FileSystemAccessMode::Write,
        )]);
        let sandbox_context = sandbox_context_with_cwd(&policy, cwd.clone());

        let err = sandbox_cwd(&sandbox_context).expect_err("non-native cwd should be rejected");

        assert_eq!(
            err,
            crate::rpc::invalid_request(format!(
                "'{cwd}' is invalid on '{}'",
                std::env::consts::OS
            ))
        );
    }

    #[test]
    fn sandbox_cwd_rejects_cwd_dependent_profile_without_context_cwd() {
        let policy = FileSystemSandboxPolicy::restricted(vec![FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::project_roots(/*subpath*/ None),
            },
            access: FileSystemAccessMode::Write,
            missing_path_behavior: None,
        }]);
        let sandbox_context = codex_file_system::FileSystemSandboxContext::from_permission_profile(
            PermissionProfile::from_runtime_permissions(&policy, NetworkSandboxPolicy::Restricted),
        );

        let err = sandbox_cwd(&sandbox_context).expect_err("missing cwd should be rejected");

        assert_eq!(
            err.message,
            "file system sandbox context with dynamic permissions requires cwd"
        );
    }

    #[test]
    fn helper_permissions_include_helper_read_root_without_additional_permissions() {
        let codex_self_exe = std::env::current_exe().expect("current exe");
        let runtime_paths =
            ExecServerRuntimePaths::new(codex_self_exe, /*codex_linux_sandbox_exe*/ None)
                .expect("runtime paths");
        let cwd = AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path())
            .expect("absolute cwd");
        let mut policy = restricted_policy(Vec::new());
        let readable = AbsolutePathBuf::from_absolute_path(
            runtime_paths
                .codex_self_exe
                .parent()
                .expect("current exe parent"),
        )
        .expect("absolute readable path");

        add_helper_runtime_permissions(
            &mut policy,
            &helper_read_roots(&runtime_paths.codex_self_exe, None),
            cwd.as_path(),
        );

        assert!(policy.can_read_path_with_cwd(readable.as_path(), cwd.as_path()));
    }

    #[test]
    fn helper_permissions_include_linux_sandbox_alias_parent() {
        let root = tempfile::tempdir().expect("temp dir");
        let codex_self_exe = root.path().join("bin").join("codex");
        let codex_linux_sandbox_exe = root.path().join("aliases").join("codex-linux-sandbox");
        let runtime_paths =
            ExecServerRuntimePaths::new(codex_self_exe, Some(codex_linux_sandbox_exe))
                .expect("runtime paths");
        let cwd = AbsolutePathBuf::from_absolute_path(std::env::temp_dir().as_path())
            .expect("absolute cwd");
        let mut policy = restricted_policy(Vec::new());
        let codex_parent = AbsolutePathBuf::from_absolute_path(root.path().join("bin"))
            .expect("absolute codex parent");
        let alias_parent = AbsolutePathBuf::from_absolute_path(root.path().join("aliases"))
            .expect("absolute alias parent");

        add_helper_runtime_permissions(
            &mut policy,
            &helper_read_roots(
                &runtime_paths.codex_self_exe,
                runtime_paths.codex_linux_sandbox_exe.as_ref(),
            ),
            cwd.as_path(),
        );

        assert!(policy.can_read_path_with_cwd(codex_parent.as_path(), cwd.as_path()));
        assert!(policy.can_read_path_with_cwd(alias_parent.as_path(), cwd.as_path()));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_request_file_writes_payload() {
        let root = tempfile::tempdir().expect("temp dir");
        let helper = root.path().join("codex.exe");
        std::fs::write(&helper, b"helper").expect("write helper");
        let request_json = br#"{"operation":"fs/readFile"}"#;

        let request_file = write_windows_fs_helper_request_file(
            helper.to_str().expect("utf-8 helper path"),
            request_json,
        )
        .expect("request file");

        assert_eq!(
            std::fs::read(request_file.path()).expect("read request file"),
            request_json
        );
        let path = request_file.path().to_owned();
        request_file.cleanup().expect("clean up request file");
        assert!(!path.exists());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_request_file_cleans_up_after_partial_write_failure() {
        let root = tempfile::tempdir().expect("temp dir");
        let helper = root.path().join("codex.exe");
        std::fs::write(&helper, b"helper").expect("write helper");

        let err = write_windows_fs_helper_request_file_with(
            helper.to_str().expect("utf-8 helper path"),
            |file| {
                std::io::Write::write_all(file, b"sensitive request prefix")?;
                Err(std::io::Error::other("injected write failure"))
            },
        )
        .expect_err("partial write should fail");

        assert!(err.message.contains("injected write failure"));
        assert!(windows_request_files(root.path()).is_empty());
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_request_file_cleans_up_after_setup_failure() {
        let root = tempfile::tempdir().expect("temp dir");
        let helper = root.path().join("codex.exe");
        std::fs::write(&helper, b"helper").expect("write helper");
        let mut request_path = None;

        let setup_result: Result<(), ()> = (|| {
            let request_file = write_windows_fs_helper_request_file(
                helper.to_str().expect("utf-8 helper path"),
                b"sensitive request",
            )
            .expect("request file");
            request_path = Some(request_file.path().to_owned());
            Err(())
        })();

        assert_eq!(setup_result, Err(()));
        assert!(!request_path.expect("request path").exists());
    }

    #[cfg(target_os = "windows")]
    #[tokio::test]
    async fn windows_request_file_cleans_up_when_owning_future_is_cancelled() {
        let root = tempfile::tempdir().expect("temp dir");
        let helper = root.path().join("codex.exe");
        std::fs::write(&helper, b"helper").expect("write helper");
        let (path_tx, path_rx) = tokio::sync::oneshot::channel();

        let task = tokio::spawn(async move {
            let request_file = write_windows_fs_helper_request_file(
                helper.to_str().expect("utf-8 helper path"),
                b"sensitive request",
            )
            .expect("request file");
            path_tx
                .send(request_file.path().to_owned())
                .expect("send request path");
            std::future::pending::<()>().await;
            drop(request_file);
        });
        let request_path = path_rx.await.expect("receive request path");
        assert!(request_path.exists());

        task.abort();
        assert!(
            task.await
                .expect_err("task should be cancelled")
                .is_cancelled()
        );
        assert!(!request_path.exists());
    }

    #[cfg(target_os = "windows")]
    fn windows_request_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(dir)
            .expect("read request directory")
            .filter_map(|entry| {
                let path = entry.expect("request directory entry").path();
                path.file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.starts_with(".codex-fs-helper-request-"))
                    .then_some(path)
            })
            .collect()
    }

    fn restricted_policy(entries: Vec<FileSystemSandboxEntry>) -> FileSystemSandboxPolicy {
        FileSystemSandboxPolicy::restricted(entries)
    }

    fn sandbox_context_with_cwd(
        policy: &FileSystemSandboxPolicy,
        cwd: PathUri,
    ) -> crate::FileSystemSandboxContext {
        codex_file_system::FileSystemSandboxContext::from_permission_profile_with_cwd(
            PermissionProfile::from_runtime_permissions(policy, NetworkSandboxPolicy::Restricted),
            cwd,
        )
    }

    fn non_native_cwd() -> PathUri {
        #[cfg(unix)]
        let uri = "file://server/share/checkout";
        #[cfg(windows)]
        let uri = "file:///usr/local/checkout";

        PathUri::parse(uri).expect("non-native cwd URI")
    }

    fn path_entry(path: AbsolutePathBuf, access: FileSystemAccessMode) -> FileSystemSandboxEntry {
        FileSystemSandboxEntry {
            path: FileSystemPath::Path { path },
            access,
            missing_path_behavior: None,
        }
    }

    fn special_entry(
        value: FileSystemSpecialPath,
        access: FileSystemAccessMode,
    ) -> FileSystemSandboxEntry {
        FileSystemSandboxEntry {
            path: FileSystemPath::Special { value },
            access,
            missing_path_behavior: None,
        }
    }
}
