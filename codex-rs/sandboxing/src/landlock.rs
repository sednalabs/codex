use codex_network_proxy::ManagedNetworkSandboxContext;
use codex_protocol::models::PermissionProfile;
use std::path::Path;

/// Basename used when the Codex executable self-invokes as the Linux sandbox
/// helper.
pub const CODEX_LINUX_SANDBOX_ARG0: &str = "codex-linux-sandbox";

pub fn allow_network_for_proxy(enforce_managed_network: bool) -> bool {
    // When managed network requirements are active, request proxy-only
    // networking from the Linux sandbox helper. Without managed requirements,
    // preserve existing behavior.
    enforce_managed_network
}

/// Converts the permission profile into the CLI invocation for
/// `codex-linux-sandbox`.
///
/// A managed context selects proxy isolation; a default context keeps its
/// restrictive policy when an older caller supplies no additional details.
///
/// The helper performs the actual sandboxing (bubblewrap by default + seccomp)
/// after parsing these arguments. The profile JSON flag is emitted before
/// helper feature flags so the argv order matches the helper's CLI shape. See
/// `docs/linux_sandbox.md` for the Linux semantics.
pub fn create_linux_sandbox_command_args_for_permission_profile(
    command: Vec<String>,
    command_cwd: &Path,
    permission_profile: &PermissionProfile,
    sandbox_policy_cwd: &Path,
    use_legacy_landlock: bool,
    managed_network: Option<&ManagedNetworkSandboxContext>,
) -> Vec<String> {
    let permission_profile_json = serde_json::to_string(permission_profile)
        .unwrap_or_else(|err| panic!("failed to serialize permission profile: {err}"));
    let sandbox_policy_cwd_arg = sandbox_policy_cwd
        .to_str()
        .unwrap_or_else(|| panic!("cwd must be valid UTF-8"))
        .to_string();
    let command_cwd = command_cwd
        .to_str()
        .unwrap_or_else(|| panic!("command cwd must be valid UTF-8"))
        .to_string();

    let mut linux_cmd: Vec<String> = vec![
        "--sandbox-policy-cwd".to_string(),
        sandbox_policy_cwd_arg,
        "--command-cwd".to_string(),
        command_cwd,
        "--permission-profile".to_string(),
        permission_profile_json,
    ];
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    if should_use_legacy_landlock_for_permission_profile(
        permission_profile,
        sandbox_policy_cwd,
        use_legacy_landlock,
        allow_network_for_proxy,
    ) {
=======
    // Proxy-only networking requires bubblewrap's isolated network namespace.
    if use_legacy_landlock && managed_network.is_none() {
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
        linux_cmd.push("--use-legacy-landlock".to_string());
    }
    if let Some(managed_network) = managed_network {
        linux_cmd.push("--managed-network".to_string());
        linux_cmd.push(
            serde_json::to_string(managed_network)
                .unwrap_or_else(|err| panic!("failed to serialize managed network context: {err}")),
        );
    }
    linux_cmd.push("--".to_string());
    linux_cmd.extend(command);
    linux_cmd
}

pub fn should_use_legacy_landlock_for_permission_profile(
    permission_profile: &PermissionProfile,
    sandbox_policy_cwd: &Path,
    use_legacy_landlock: bool,
    allow_network_for_proxy: bool,
) -> bool {
    if !use_legacy_landlock || allow_network_for_proxy {
        return false;
    }

    let (file_system_sandbox_policy, network_sandbox_policy) =
        permission_profile.to_runtime_permissions();
    !file_system_sandbox_policy
        .needs_direct_runtime_enforcement(network_sandbox_policy, sandbox_policy_cwd)
}

/// Converts the sandbox cwd and execution options into the CLI invocation for
/// `codex-linux-sandbox`.
#[cfg_attr(not(test), allow(dead_code))]
fn create_linux_sandbox_command_args(
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
    // Proxy-only networking requires bubblewrap's isolated network namespace.
    if use_legacy_landlock && !allow_network_for_proxy {
        linux_cmd.push("--use-legacy-landlock".to_string());
    }
    if allow_network_for_proxy {
        linux_cmd.push("--managed-network".to_string());
        linux_cmd.push("{}".to_string());
    }

    // Separator so that command arguments starting with `-` are not parsed as
    // options of the helper itself.
    linux_cmd.push("--".to_string());

    // Append the original tool command.
    linux_cmd.extend(command);

    linux_cmd
}

#[cfg(test)]
#[path = "landlock_tests.rs"]
mod tests;
