use super::*;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::protocol::SandboxPolicy;
use codex_utils_absolute_path::AbsolutePathBuf;
use pretty_assertions::assert_eq;
use tempfile::tempdir;

#[test]
fn legacy_landlock_flag_is_included_when_requested() {
    let command = vec!["/bin/true".to_string()];
    let command_cwd = Path::new("/tmp/link");
    let cwd = Path::new("/tmp");

    let default_bwrap = create_linux_sandbox_command_args(
        command.clone(),
        command_cwd,
        cwd,
        /*use_legacy_landlock*/ false,
        /*allow_network_for_proxy*/ false,
    );
    assert_eq!(
        default_bwrap.contains(&"--use-legacy-landlock".to_string()),
        false
    );

    let legacy_landlock = create_linux_sandbox_command_args(
        command,
        command_cwd,
        cwd,
        /*use_legacy_landlock*/ true,
        /*allow_network_for_proxy*/ false,
    );
    assert_eq!(
        legacy_landlock.contains(&"--use-legacy-landlock".to_string()),
        true
    );
}

#[test]
fn proxy_flag_is_included_when_requested() {
    let command = vec!["/bin/true".to_string()];
    let command_cwd = Path::new("/tmp/link");
    let cwd = Path::new("/tmp");

    let args = create_linux_sandbox_command_args(
        command,
        command_cwd,
        cwd,
        /*use_legacy_landlock*/ true,
        /*allow_network_for_proxy*/ true,
    );
    assert_eq!(
        args.contains(&"--allow-network-for-proxy".to_string()),
        true
    );
}

#[test]
fn split_policy_flags_are_included() {
    let command = vec!["/bin/true".to_string()];
    let command_cwd = Path::new("/tmp/link");
    let cwd = Path::new("/tmp");
    let sandbox_policy = SandboxPolicy::new_read_only_policy();
    let file_system_sandbox_policy = FileSystemSandboxPolicy::from(&sandbox_policy);
    let network_sandbox_policy = NetworkSandboxPolicy::from(&sandbox_policy);

    let args = create_linux_sandbox_command_args_for_policies(
        command,
        command_cwd,
        &sandbox_policy,
        &file_system_sandbox_policy,
        network_sandbox_policy,
        cwd,
        /*use_legacy_landlock*/ true,
        /*allow_network_for_proxy*/ false,
    );

    assert_eq!(
        args.windows(2)
            .any(|window| { window[0] == "--file-system-sandbox-policy" && !window[1].is_empty() }),
        true
    );
    assert_eq!(
        args.windows(2)
            .any(|window| window[0] == "--network-sandbox-policy" && window[1] == "\"restricted\""),
        true
    );
    assert_eq!(
        args.windows(2)
            .any(|window| window[0] == "--command-cwd" && window[1] == "/tmp/link"),
        true
    );
}

#[test]
fn incompatible_split_policies_skip_legacy_landlock_flag() {
    let cwd = tempdir().expect("tempdir");
    let command = vec!["/bin/true".to_string()];
    let nested = AbsolutePathBuf::try_from(cwd.path().join("nested")).expect("absolute nested");
    let command_cwd = nested.as_path();
    let sandbox_policy = SandboxPolicy::WorkspaceWrite {
        writable_roots: vec![nested.clone()],
        network_access: false,
        exclude_tmpdir_env_var: true,
        exclude_slash_tmp: true,
    };
    let file_system_sandbox_policy = FileSystemSandboxPolicy::restricted(vec![
        FileSystemSandboxEntry {
            path: FileSystemPath::Special {
                value: FileSystemSpecialPath::CurrentWorkingDirectory,
            },
            access: FileSystemAccessMode::Write,
        },
        FileSystemSandboxEntry {
            path: FileSystemPath::Path {
                path: nested.clone(),
            },
            access: FileSystemAccessMode::Read,
        },
    ]);
    let network_sandbox_policy = NetworkSandboxPolicy::Restricted;

    let args = create_linux_sandbox_command_args_for_policies(
        command,
        command_cwd,
        &sandbox_policy,
        &file_system_sandbox_policy,
        network_sandbox_policy,
        cwd.path(),
        /*use_legacy_landlock*/ true,
        /*allow_network_for_proxy*/ false,
    );

    assert_eq!(args.contains(&"--use-legacy-landlock".to_string()), false);
}

#[test]
fn proxy_network_requires_managed_requirements() {
    assert_eq!(
        allow_network_for_proxy(/*enforce_managed_network*/ false),
        false
    );
    assert_eq!(
        allow_network_for_proxy(/*enforce_managed_network*/ true),
        true
    );
}
