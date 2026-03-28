use crate::protocol::SandboxPolicy;
use crate::spawn::SpawnChildRequest;
use crate::spawn::StdioPolicy;
use crate::spawn::spawn_child_async;
use codex_network_proxy::NetworkProxy;
use codex_protocol::permissions::FileSystemSandboxPolicy;
use codex_protocol::permissions::NetworkSandboxPolicy;
use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::sync::OnceLock;
use tokio::process::Child;

/// Spawn a shell tool command under the Linux sandbox helper
/// (codex-linux-sandbox), which defaults to bubblewrap for filesystem
/// isolation plus seccomp for network restrictions.
///
/// Unlike macOS Seatbelt where we directly embed the policy text, the Linux
/// helper is a separate executable. We pass the legacy [`SandboxPolicy`] plus
/// split filesystem/network policies as JSON so the helper can migrate
/// incrementally without breaking older call sites.
#[allow(clippy::too_many_arguments)]
pub async fn spawn_command_under_linux_sandbox<P>(
    codex_linux_sandbox_exe: P,
    command: Vec<String>,
    command_cwd: PathBuf,
    sandbox_policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    use_legacy_landlock: bool,
    stdio_policy: StdioPolicy,
    network: Option<&NetworkProxy>,
    env: HashMap<String, String>,
) -> std::io::Result<Child>
where
    P: AsRef<Path>,
{
    let file_system_sandbox_policy =
        FileSystemSandboxPolicy::from_legacy_sandbox_policy(sandbox_policy, sandbox_policy_cwd);
    let network_sandbox_policy = NetworkSandboxPolicy::from(sandbox_policy);
    let allow_network_for_proxy =
        allow_network_for_proxy(/*enforce_managed_network*/ false);
    let supports_split_policy =
        linux_sandbox_supports_split_policy_flags(codex_linux_sandbox_exe.as_ref());
    let use_legacy_landlock = use_legacy_landlock
        && !(supports_split_policy
            && file_system_sandbox_policy
                .needs_direct_runtime_enforcement(network_sandbox_policy, sandbox_policy_cwd));
    let args = if supports_split_policy {
        create_linux_sandbox_command_args_for_policies(
            command,
            command_cwd.as_path(),
            sandbox_policy,
            &file_system_sandbox_policy,
            network_sandbox_policy,
            sandbox_policy_cwd,
            use_legacy_landlock,
            allow_network_for_proxy,
        )
    } else {
        create_linux_sandbox_command_args_legacy(
            command,
            sandbox_policy,
            sandbox_policy_cwd,
            use_legacy_landlock,
            allow_network_for_proxy,
        )
    };
    let arg0 = Some("codex-linux-sandbox");
    spawn_child_async(SpawnChildRequest {
        program: codex_linux_sandbox_exe.as_ref().to_path_buf(),
        args,
        arg0,
        cwd: command_cwd,
        network_sandbox_policy,
        network,
        stdio_policy,
        env,
    })
    .await
}

pub(crate) fn allow_network_for_proxy(enforce_managed_network: bool) -> bool {
    // When managed network requirements are active, request proxy-only
    // networking from the Linux sandbox helper. Without managed requirements,
    // preserve existing behavior.
    enforce_managed_network
}

pub(crate) fn linux_sandbox_supports_split_policy_flags(codex_linux_sandbox_exe: &Path) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut cached = cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(supports_split_policy) = cached.get(codex_linux_sandbox_exe) {
        return *supports_split_policy;
    }

    let sandbox_policy = SandboxPolicy::new_read_only_policy();
    let file_system_sandbox_policy =
        FileSystemSandboxPolicy::from_legacy_sandbox_policy(&sandbox_policy, Path::new("/tmp"));
    let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);
    let sandbox_policy_json = serde_json::to_string(&sandbox_policy)
        .unwrap_or_else(|err| panic!("failed to serialize sandbox policy: {err}"));
    let file_system_policy_json = serde_json::to_string(&file_system_sandbox_policy)
        .unwrap_or_else(|err| panic!("failed to serialize filesystem sandbox policy: {err}"));
    let network_policy_json = serde_json::to_string(&network_sandbox_policy)
        .unwrap_or_else(|err| panic!("failed to serialize network sandbox policy: {err}"));

    let mut command = Command::new(codex_linux_sandbox_exe);
    #[cfg(target_os = "linux")]
    command.arg0("codex-linux-sandbox");
    let supports_split_policy = command
        .arg("--sandbox-policy-cwd")
        .arg("/tmp")
        .arg("--sandbox-policy")
        .arg(sandbox_policy_json)
        .arg("--file-system-sandbox-policy")
        .arg(file_system_policy_json)
        .arg("--network-sandbox-policy")
        .arg(network_policy_json)
        .arg("--")
        .arg("/bin/true")
        .output()
        .is_ok_and(|output| output.status.success());

    cached.insert(codex_linux_sandbox_exe.to_path_buf(), supports_split_policy);
    supports_split_policy
}

pub(crate) fn create_linux_sandbox_command_args_legacy(
    command: Vec<String>,
    sandbox_policy: &SandboxPolicy,
    sandbox_policy_cwd: &Path,
    use_legacy_landlock: bool,
    allow_network_for_proxy: bool,
) -> Vec<String> {
    let sandbox_policy_json = serde_json::to_string(sandbox_policy)
        .unwrap_or_else(|err| panic!("failed to serialize sandbox policy: {err}"));
    let sandbox_policy_cwd = sandbox_policy_cwd
        .to_str()
        .unwrap_or_else(|| panic!("cwd must be valid UTF-8"))
        .to_string();

    let mut linux_cmd: Vec<String> = vec![
        "--sandbox-policy-cwd".to_string(),
        sandbox_policy_cwd,
        "--sandbox-policy".to_string(),
        sandbox_policy_json,
    ];
    if use_legacy_landlock {
        linux_cmd.push("--use-legacy-landlock".to_string());
    }
    if allow_network_for_proxy {
        linux_cmd.push("--allow-network-for-proxy".to_string());
    }
    linux_cmd.push("--".to_string());
    linux_cmd.extend(command);
    linux_cmd
}

/// Converts the sandbox policies into the CLI invocation for
/// `codex-linux-sandbox`.
///
/// The helper performs the actual sandboxing (bubblewrap by default + seccomp) after
/// parsing these arguments. Policy JSON flags are emitted before helper feature
/// flags so the argv order matches the helper's CLI shape. See
/// `docs/linux_sandbox.md` for the Linux semantics.
#[allow(clippy::too_many_arguments)]
pub fn create_linux_sandbox_command_args_for_policies(
    command: Vec<String>,
    command_cwd: &Path,
    sandbox_policy: &SandboxPolicy,
    file_system_sandbox_policy: &FileSystemSandboxPolicy,
    network_sandbox_policy: NetworkSandboxPolicy,
    sandbox_policy_cwd: &Path,
    use_legacy_landlock: bool,
    allow_network_for_proxy: bool,
) -> Vec<String> {
    let sandbox_policy_json = serde_json::to_string(sandbox_policy)
        .unwrap_or_else(|err| panic!("failed to serialize sandbox policy: {err}"));
    let file_system_policy_json = serde_json::to_string(file_system_sandbox_policy)
        .unwrap_or_else(|err| panic!("failed to serialize filesystem sandbox policy: {err}"));
    let network_policy_json = serde_json::to_string(&network_sandbox_policy)
        .unwrap_or_else(|err| panic!("failed to serialize network sandbox policy: {err}"));
    let sandbox_policy_cwd = sandbox_policy_cwd
        .to_str()
        .unwrap_or_else(|| panic!("cwd must be valid UTF-8"))
        .to_string();
    let command_cwd = command_cwd
        .to_str()
        .unwrap_or_else(|| panic!("command cwd must be valid UTF-8"))
        .to_string();

    let mut linux_cmd: Vec<String> = vec![
        "--sandbox-policy-cwd".to_string(),
        sandbox_policy_cwd,
        "--command-cwd".to_string(),
        command_cwd,
        "--sandbox-policy".to_string(),
        sandbox_policy_json,
        "--file-system-sandbox-policy".to_string(),
        file_system_policy_json,
        "--network-sandbox-policy".to_string(),
        network_policy_json,
    ];
    if use_legacy_landlock {
        linux_cmd.push("--use-legacy-landlock".to_string());
    }
    if allow_network_for_proxy {
        linux_cmd.push("--allow-network-for-proxy".to_string());
    }
    linux_cmd.push("--".to_string());
    linux_cmd.extend(command);
    linux_cmd
}

/// Converts the sandbox cwd and execution options into the CLI invocation for
/// `codex-linux-sandbox`.
#[cfg(test)]
pub(crate) fn create_linux_sandbox_command_args(
    command: Vec<String>,
    command_cwd: &Path,
    sandbox_policy_cwd: &Path,
    use_legacy_landlock: bool,
    allow_network_for_proxy: bool,
) -> Vec<String> {
    let command_cwd = command_cwd
        .to_str()
        .unwrap_or_else(|| panic!("command cwd must be valid UTF-8"))
        .to_string();
    let sandbox_policy_cwd = sandbox_policy_cwd
        .to_str()
        .unwrap_or_else(|| panic!("cwd must be valid UTF-8"))
        .to_string();

    let mut linux_cmd: Vec<String> = vec![
        "--sandbox-policy-cwd".to_string(),
        sandbox_policy_cwd,
        "--command-cwd".to_string(),
        command_cwd,
    ];
    if use_legacy_landlock {
        linux_cmd.push("--use-legacy-landlock".to_string());
    }
    if allow_network_for_proxy {
        linux_cmd.push("--allow-network-for-proxy".to_string());
    }

    // Separator so that command arguments starting with `-` are not parsed as
    // options of the helper itself.
    linux_cmd.push("--".to_string());

    // Append the original tool command.
    linux_cmd.extend(command);

    linux_cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn legacy_landlock_flag_is_included_when_requested() {
        let command = vec!["/bin/true".to_string()];
        let cwd = Path::new("/tmp");

        let default_bwrap =
            create_linux_sandbox_command_args(command.clone(), cwd, cwd, false, false);
        assert_eq!(
            default_bwrap.contains(&"--use-legacy-landlock".to_string()),
            false
        );

        let legacy_landlock = create_linux_sandbox_command_args(command, cwd, cwd, true, false);
        assert_eq!(
            legacy_landlock.contains(&"--use-legacy-landlock".to_string()),
            true
        );
    }

    #[test]
    fn proxy_flag_is_included_when_requested() {
        let command = vec!["/bin/true".to_string()];
        let cwd = Path::new("/tmp");

        let args = create_linux_sandbox_command_args(command, cwd, cwd, true, true);
        assert_eq!(
            args.contains(&"--allow-network-for-proxy".to_string()),
            true
        );
    }

    #[test]
    fn legacy_policy_args_exclude_split_flags() {
        let command = vec!["/bin/true".to_string()];
        let cwd = Path::new("/tmp");
        let sandbox_policy = SandboxPolicy::new_read_only_policy();

        let args =
            create_linux_sandbox_command_args_legacy(command, &sandbox_policy, cwd, true, true);

        assert_eq!(
            args.windows(2)
                .any(|window| window[0] == "--sandbox-policy" && !window[1].is_empty()),
            true
        );
        assert_eq!(
            args.contains(&"--file-system-sandbox-policy".to_string()),
            false
        );
        assert_eq!(
            args.contains(&"--network-sandbox-policy".to_string()),
            false
        );
    }

    #[test]
    fn split_policy_flags_are_included() {
        let command = vec!["/bin/true".to_string()];
        let cwd = Path::new("/tmp");
        let sandbox_policy = SandboxPolicy::new_read_only_policy();
        let file_system_sandbox_policy = FileSystemSandboxPolicy::from(&sandbox_policy);
        let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);

        let args = create_linux_sandbox_command_args_for_policies(
            command,
            cwd,
            &sandbox_policy,
            &file_system_sandbox_policy,
            network_sandbox_policy,
            cwd,
            true,
            false,
        );

        assert_eq!(
            args.windows(2).any(|window| {
                window[0] == "--file-system-sandbox-policy" && !window[1].is_empty()
            }),
            true
        );
        assert_eq!(
            args.windows(2)
                .any(|window| window[0] == "--network-sandbox-policy"
                    && window[1] == "\"restricted\""),
            true
        );
    }

    #[test]
    fn proxy_network_requires_managed_requirements() {
        assert_eq!(allow_network_for_proxy(false), false);
        assert_eq!(allow_network_for_proxy(true), true);
    }
}
