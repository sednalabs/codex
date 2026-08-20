/*
Module: runtimes

Concrete ToolRuntime implementations for specific tools. Each runtime stays
small and focused and reuses the orchestrator for approvals + sandbox + retry.
*/
use crate::exec_env::CODEX_PERMISSION_PROFILE_ENV_VAR;
use crate::exec_env::CODEX_THREAD_ID_ENV_VAR;
use crate::sandboxing::SandboxPermissions;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::tools::sandboxing::ToolError;
use codex_apply_patch::CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR;
#[cfg(unix)]
use codex_install_context::InstallContext;
#[cfg(target_os = "macos")]
use codex_network_proxy::CODEX_PROXY_GIT_SSH_COMMAND_MARKER;
use codex_network_proxy::CUSTOM_CA_ENV_KEYS;
use codex_network_proxy::PROXY_ACTIVE_ENV_KEY;
use codex_network_proxy::PROXY_ENV_KEYS;
#[cfg(target_os = "macos")]
use codex_network_proxy::PROXY_GIT_SSH_COMMAND_ENV_KEY;
pub(crate) use codex_network_proxy::is_managed_proxy_env_var;
pub(crate) use codex_network_proxy::strip_managed_proxy_env;
use codex_protocol::config_types::WindowsSandboxLevel;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_sandboxing::SandboxCommand;
use codex_sandboxing::SandboxType;
use codex_sandboxing::windows_sandbox_uses_elevated_backend;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_path_uri::PathUri;
use std::collections::HashMap;
#[cfg(unix)]
use std::path::Path;

pub(crate) mod apply_patch;
pub(crate) mod shell;
pub(crate) mod unified_exec;

/// Shared helper to construct sandbox transform inputs from a tokenized command line and native
/// working directory. Validates that at least a program is present.
pub(crate) fn build_sandbox_command(
    command: &[String],
    cwd: &AbsolutePathBuf,
    env: &HashMap<String, String>,
    additional_permissions: Option<AdditionalPermissionProfile>,
) -> Result<SandboxCommand, ToolError> {
    let (program, args) = command
        .split_first()
        .ok_or_else(|| ToolError::Rejected("command args are empty".to_string()))?;
    let cwd = PathUri::from_abs_path(cwd);
    Ok(SandboxCommand {
        program: program.clone().into(),
        args: args.to_vec(),
        cwd,
        env: env.clone(),
        managed_network: None,
        additional_permissions,
    })
}

pub(crate) fn exec_env_for_sandbox_permissions(
    env: &HashMap<String, String>,
    sandbox_permissions: SandboxPermissions,
) -> HashMap<String, String> {
    let mut env = env.clone();
    if sandbox_permissions.requires_escalated_permissions()
        && env.contains_key(PROXY_ACTIVE_ENV_KEY)
    {
        strip_managed_proxy_env(&mut env);
    }
    env
}

/// Prepends `path_entry` to `PATH`, removing duplicate and empty existing
/// entries.
///
/// Returns the updated `PATH` value when `env` was changed. Returns `None` when
/// `path_entry` is empty, leaving `env` untouched so an empty entry does not add
/// the current working directory to command lookup.
#[cfg(unix)]
fn prepend_path_entry(env: &mut HashMap<String, String>, path_entry: &str) -> Option<String> {
    if path_entry.is_empty() {
        None
    } else {
        let updated_path = match env.get("PATH") {
            Some(path) if !path.is_empty() => std::iter::once(path_entry)
                .chain(
                    path.split(':')
                        .filter(|entry| !entry.is_empty() && *entry != path_entry),
                )
                .collect::<Vec<_>>()
                .join(":"),
            _ => path_entry.to_string(),
        };
        env.insert("PATH".to_string(), updated_path.clone());
        Some(updated_path)
    }
}

/// PATH entries owned by Codex runtime setup.
///
/// These are applied to the live exec environment immediately and replayed after
/// restoring a shell snapshot, unless the user explicitly overrides `PATH`.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct RuntimePathPrepends {
    entries: Vec<String>,
}

impl RuntimePathPrepends {
    #[cfg(unix)]
    pub(crate) fn prepend(&mut self, env: &mut HashMap<String, String>, path_entry: &Path) {
        let path_entry = path_entry.to_string_lossy().to_string();
        if prepend_path_entry(env, &path_entry).is_some() {
            self.entries.retain(|entry| entry != &path_entry);
            self.entries.push(path_entry);
        }
    }

    fn shell_exports_after_snapshot(
        &self,
        explicit_env_overrides: &HashMap<String, String>,
    ) -> String {
        if explicit_env_overrides.contains_key("PATH") {
            return String::new();
        }

        self.entries
            .iter()
            .filter(|entry| !entry.is_empty())
            .map(|entry| {
                let entry = shell_single_quote(entry);
                format!(
                    "if [ -n \"${{PATH:-}}\" ]; then export PATH='{entry}':\"$PATH\"; else export PATH='{entry}'; fi"
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(unix)]
pub(crate) fn apply_package_path_prepend(
    env: &mut HashMap<String, String>,
    runtime_path_prepends: &mut RuntimePathPrepends,
) {
    let Some(path_dir) = InstallContext::current()
        .package_layout
        .as_ref()
        .and_then(|package_layout| package_layout.path_dir.as_ref())
    else {
        return;
    };

    runtime_path_prepends.prepend(env, path_dir.as_path());
}

#[cfg(unix)]
pub(crate) fn prepend_zsh_fork_bin_to_path(
    env: &mut HashMap<String, String>,
    shell_zsh_path: &Path,
) -> Option<String> {
    let zsh_bin_dir = shell_zsh_path
        .parent()
        .map(|path| path.to_string_lossy().to_string())?;
    prepend_path_entry(env, &zsh_bin_dir)
}

#[cfg(unix)]
pub(crate) fn apply_zsh_fork_path_prepend(
    env: &mut HashMap<String, String>,
    runtime_path_prepends: &mut RuntimePathPrepends,
    shell_zsh_path: &Path,
) {
    let Some(zsh_bin_dir) = shell_zsh_path.parent() else {
        return;
    };
    runtime_path_prepends.prepend(env, zsh_bin_dir);
}

pub(crate) fn disable_powershell_profile_for_elevated_windows_sandbox(
    command: &[String],
    shell_type: Option<&ShellType>,
    sandbox: SandboxType,
    windows_sandbox_level: WindowsSandboxLevel,
    proxy_enforced: bool,
) -> Vec<String> {
    if shell_type != Some(&ShellType::PowerShell)
        || sandbox != SandboxType::WindowsRestrictedToken
        || !windows_sandbox_uses_elevated_backend(windows_sandbox_level, proxy_enforced)
        || command.is_empty()
    {
        return command.to_vec();
    }

    if command[1..]
        .iter()
        .any(|arg| arg.eq_ignore_ascii_case("-NoProfile"))
    {
        return command.to_vec();
    }

    // The elevated Windows sandbox runs as a dedicated sandbox account while
    // HOME/USERPROFILE may still point at the real user profile. Loading
    // PowerShell profiles in that mixed context is not a valid login shell.
    let mut command = command.to_vec();
    command.insert(1, "-NoProfile".to_string());
    command
}

pub(crate) fn scrub_shell_startup_hook_env_vars(env: &mut HashMap<String, String>) {
    env.retain(|name, _| {
        !matches!(
            name.to_ascii_uppercase().as_str(),
            "ENV" | "BASH_ENV" | "ZDOTDIR"
        )
    });
}

/// POSIX-only helper: for commands produced by `Shell::derive_exec_args`
/// for Bash/Zsh/sh of the form `[shell_path, "-lc", "<script>"]`, and
/// when a snapshot is configured on the session shell, rewrite the argv
/// to a single non-login shell that sources the snapshot before running
/// the original script:
///
///   shell -lc "<script>"
///   => user_shell -c ". SNAPSHOT (best effort); exec shell -c <script>"
///
/// This wrapper script uses POSIX constructs (`if`, `.`, `exec`) so it can
/// be run by Bash/Zsh/sh. On non-matching commands, or when command cwd does
/// not match the snapshot cwd, this is a no-op.
///
/// `explicit_env_overrides` and `env` are intentionally separate inputs.
/// `explicit_env_overrides` contains policy-driven shell env overrides that
/// should win after the snapshot is sourced, while `env` is the full live exec
/// environment. We need access to both so snapshot restore logic can preserve
/// runtime-only vars like `CODEX_THREAD_ID` without pretending they came from
/// the explicit override policy.
///
/// `runtime_path_prepends` contains Codex-owned PATH entries already applied to
/// the live `env`; snapshot wrapping replays them after restoring the snapshot
/// PATH unless the user explicitly overrides `PATH`.
pub(crate) fn maybe_wrap_shell_lc_with_snapshot(
    command: &[String],
    session_shell: &Shell,
    shell_snapshot: Option<&AbsolutePathBuf>,
    explicit_env_overrides: &HashMap<String, String>,
    env: &HashMap<String, String>,
    runtime_path_prepends: &RuntimePathPrepends,
) -> Vec<String> {
    if cfg!(windows) {
        return command.to_vec();
    }

    let Some(snapshot) = shell_snapshot else {
        return command.to_vec();
    };

    if !snapshot.exists() {
        return command.to_vec();
    }

    if command.len() < 3 {
        return command.to_vec();
    }

    let flag = command[1].as_str();
    if flag != "-lc" {
        return command.to_vec();
    }

    let shell_path = session_shell.shell_path.to_string_lossy().into_owned();
    let mut override_env = explicit_env_overrides
        .iter()
        .filter(|(key, _)| !is_protected_snapshot_override(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<HashMap<_, _>>();
    for key in [
        CODEX_THREAD_ID_ENV_VAR,
        CODEX_PERMISSION_PROFILE_ENV_VAR,
        CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR,
    ] {
        if let Some(value) = env.get(key) {
            override_env.insert(key.to_string(), value.clone());
        }
    }
    // Do not let a snapshot resurrect stale runtime state when it is inactive.
    let (override_captures, override_exports) = build_override_exports(
        &override_env,
        &[
            CODEX_PERMISSION_PROFILE_ENV_VAR,
            CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR,
        ],
    );
    let (proxy_captures, proxy_exports) = build_proxy_env_exports();
    let non_inheritable_tool_captures = build_non_inheritable_env_tool_captures();
    let non_inheritable_scrub = build_non_inheritable_env_scrub_safe();
    let runtime_path_prepend_exports =
        runtime_path_prepends.shell_exports_after_snapshot(explicit_env_overrides);
    let override_captures = join_shell_blocks([
        non_inheritable_tool_captures.clone(),
        override_captures,
        proxy_captures,
    ]);
    let override_exports = join_shell_blocks([
        non_inheritable_scrub.clone(),
        override_exports,
        proxy_exports,
        runtime_path_prepend_exports,
    ]);
    let startup_hook_args = build_startup_hook_args(explicit_env_overrides);
    let inner_script = build_snapshot_inner_script();
    let target_script = build_snapshot_target_script();
    let post_startup_script = build_post_startup_script();
    let rewritten_script = if override_exports.is_empty() {
        format!(
            "{non_inheritable_tool_captures}\n\n__codex_source_snapshot() {{ . \"$1\" >/dev/null 2>&1; }}\n__codex_source_snapshot \"$1\"\n\n{non_inheritable_scrub}\n\n__codex_inner_script=$2\n__codex_target_script=$3\n__codex_post_startup_script=$4\n__codex_original_shell=$5\n__codex_original_script=$6\n__codex_env_set=$7\n__codex_env_value=$8\n__codex_bash_env_set=$9\n__codex_bash_env_value=${{10}}\n__codex_zdotdir_set=${{11}}\n__codex_zdotdir_value=${{12}}\ncommand readonly __codex_inner_script __codex_target_script __codex_post_startup_script __codex_original_shell __codex_original_script __codex_env_set __codex_env_value __codex_bash_env_set __codex_bash_env_value __codex_zdotdir_set __codex_zdotdir_value\nshift 12\nexec \"$__codex_env\" -u ENV -u BASH_ENV -u ZDOTDIR /bin/sh -c \"$__codex_inner_script\" codex-inner \"$__codex_env\" \"$__codex_awk\" \"$__codex_target_script\" \"$__codex_post_startup_script\" \"$__codex_original_shell\" \"$__codex_original_script\" \"$__codex_env_set\" \"$__codex_env_value\" \"$__codex_bash_env_set\" \"$__codex_bash_env_value\" \"$__codex_zdotdir_set\" \"$__codex_zdotdir_value\" \"$@\""
        )
    } else {
        format!(
            "{override_captures}\n\n__codex_source_snapshot() {{ . \"$1\" >/dev/null 2>&1; }}\n__codex_source_snapshot \"$1\"\n\n{override_exports}\n\n__codex_inner_script=$2\n__codex_target_script=$3\n__codex_post_startup_script=$4\n__codex_original_shell=$5\n__codex_original_script=$6\n__codex_env_set=$7\n__codex_env_value=$8\n__codex_bash_env_set=$9\n__codex_bash_env_value=${{10}}\n__codex_zdotdir_set=${{11}}\n__codex_zdotdir_value=${{12}}\ncommand readonly __codex_inner_script __codex_target_script __codex_post_startup_script __codex_original_shell __codex_original_script __codex_env_set __codex_env_value __codex_bash_env_set __codex_bash_env_value __codex_zdotdir_set __codex_zdotdir_value\nshift 12\nexec \"$__codex_env\" -u ENV -u BASH_ENV -u ZDOTDIR /bin/sh -c \"$__codex_inner_script\" codex-inner \"$__codex_env\" \"$__codex_awk\" \"$__codex_target_script\" \"$__codex_post_startup_script\" \"$__codex_original_shell\" \"$__codex_original_script\" \"$__codex_env_set\" \"$__codex_env_value\" \"$__codex_bash_env_set\" \"$__codex_bash_env_value\" \"$__codex_zdotdir_set\" \"$__codex_zdotdir_value\" \"$@\""
        )
    };

    let mut rewritten = vec![shell_path, "-c".to_string(), rewritten_script];
    rewritten.push("codex-snapshot".to_string());
    rewritten.push(snapshot.to_string_lossy().into_owned());
    rewritten.push(inner_script);
    rewritten.push(target_script);
    rewritten.push(post_startup_script);
    rewritten.push(command[0].clone());
    rewritten.push(command[2].clone());
    rewritten.extend(startup_hook_args);
    rewritten.extend(command[3..].iter().cloned());
    rewritten
}

fn build_non_inheritable_env_tool_captures() -> String {
    // Resolve these before sourcing the snapshot so a restored PATH or shell function cannot
    // redirect the scrub's command substitutions. `command -p` searches the shell's default
    // utility path rather than the snapshot-controlled PATH.
    "__codex_env=$(command -p -v env)\n__codex_awk=$(command -p -v awk)\ncommand readonly __codex_env __codex_awk".to_string()
}

#[allow(dead_code)]
fn build_non_inheritable_env_scrub() -> String {
    "__codex_scrub_script=\"$(__codex_env=\"$__codex_env\"; \"$__codex_env\" | \"$__codex_awk\" -F= 'tolower($1) == \"openai_federation_rule_id\" || tolower($1) == \"openai_identity_token_file\" { printf \"unset '\\047%s'\\047\\n\", $1 }')\"".to_string()
}

fn build_non_inheritable_env_scrub_safe() -> String {
    r#"__codex_scrub_script=$("$__codex_env" | "$__codex_awk" -F= 'tolower($1) == "openai_federation_rule_id" || tolower($1) == "openai_identity_token_file" { printf "unset %s\n", $1 }')"#
        .to_string()
}

#[allow(dead_code)]
fn build_legacy_non_inheritable_safe_exec(
    original_shell: &str,
    original_script: &str,
    trailing_args: &str,
    startup_hook_exports: &str,
    post_startup_scrub: &str,
) -> String {
    format!(
        "exec \"$__codex_env\" -u ENV -u BASH_ENV -u ZDOTDIR /bin/sh -c \"unset ENV BASH_ENV ZDOTDIR\\n$__codex_scrub_script\\n__codex_shell=\\$1\\n__codex_script=\\\"{post_startup_scrub}\\n\\$2\\\"\\nshift 2\\n{startup_hook_exports}\\nexec \\\"\\$__codex_shell\\\" -c \\\"\\$__codex_script\\\" \\\"\\$@\\\"\" sh '{original_shell}' '{original_script}'{trailing_args}"
    )
}

#[allow(dead_code)]
fn build_legacy_post_startup_scrub(
    _shell_path: &str,
    _original_shell: &str,
    _original_script: &str,
    trailing_args: &str,
) -> String {
    // Cross an exec boundary before scrubbing so readonly attributes from a
    // startup file cannot survive into the payload shell.
    format!(
        "__codex_post_scrub=\"$(\\\"$__codex_env\\\" | \\\"$__codex_awk\\\" -F= 'tolower($1) == \\\"openai_federation_rule_id\\\" || tolower($1) == \\\"openai_identity_token_file\\\" {{ print \\\"unset '\\047\\\" $1 \\\"'\\047\\\" }}')\"\nexec \"$__codex_env\" -u ENV -u BASH_ENV -u ZDOTDIR /bin/sh -c \"$__codex_post_scrub\\nexec \\\"$__codex_shell\\\" -c \\\"$__codex_script\\\" \\\"\\$@\\\"\" sh{trailing_args}"
    )
}

#[allow(dead_code)]
fn build_legacy_startup_hook_exports(explicit_env_overrides: &HashMap<String, String>) -> String {
    explicit_env_overrides
        .iter()
        .filter_map(|(name, value)| {
            let canonical = match name.to_ascii_uppercase().as_str() {
                "ENV" => "ENV",
                "BASH_ENV" => "BASH_ENV",
                "ZDOTDIR" => "ZDOTDIR",
                _ => return None,
            };
            Some(format!(
                "export {canonical}='{}'",
                shell_single_quote(value)
            ))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_snapshot_inner_script() -> String {
    r#"unset ENV BASH_ENV ZDOTDIR
__codex_env=$1
__codex_awk=$2
__codex_target_script=$3
__codex_post_startup_script=$4
__codex_original_shell=$5
__codex_original_script=$6
__codex_env_set=$7
__codex_env_value=$8
__codex_bash_env_set=$9
__codex_bash_env_value=${10}
__codex_zdotdir_set=${11}
__codex_zdotdir_value=${12}
command readonly __codex_env __codex_awk __codex_target_script __codex_post_startup_script __codex_original_shell __codex_original_script
shift 12
__codex_scrub_script=$("$__codex_env" | "$__codex_awk" -F= 'tolower($1) == "openai_federation_rule_id" || tolower($1) == "openai_identity_token_file" { printf "unset %s\n", $1 }')
eval "$__codex_scrub_script"
if [ "$__codex_env_set" = 1 ]; then export ENV="$__codex_env_value"; fi
if [ "$__codex_bash_env_set" = 1 ]; then export BASH_ENV="$__codex_bash_env_value"; fi
if [ "$__codex_zdotdir_set" = 1 ]; then export ZDOTDIR="$__codex_zdotdir_value"; fi
exec "$__codex_original_shell" -c "$__codex_target_script" codex-target "$__codex_env" "$__codex_awk" "$__codex_post_startup_script" "$__codex_original_shell" "$__codex_original_script" "$@""#
        .to_string()
}

fn build_snapshot_target_script() -> String {
    r#"exec "$1" -u ENV -u BASH_ENV -u ZDOTDIR /bin/sh -c "$3" codex-post "$1" "$2" "$4" "$5" "$@""#
        .to_string()
}

fn build_post_startup_script() -> String {
    r#"__codex_env=$1
__codex_awk=$2
__codex_original_shell=$3
__codex_original_script=$4
shift 9
__codex_scrub_script=$("$__codex_env" | "$__codex_awk" -F= 'tolower($1) == "openai_federation_rule_id" || tolower($1) == "openai_identity_token_file" { printf "unset %s\n", $1 }')
eval "$__codex_scrub_script"
exec "$__codex_original_shell" -c "$__codex_original_script" "$@""#
        .to_string()
}

fn build_startup_hook_args(explicit_env_overrides: &HashMap<String, String>) -> Vec<String> {
    ["ENV", "BASH_ENV", "ZDOTDIR"]
        .into_iter()
        .flat_map(|canonical| {
            explicit_env_overrides
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(canonical))
                .map(|(_, value)| vec!["1".to_string(), value.clone()])
                .unwrap_or_else(|| vec!["0".to_string(), String::new()])
        })
        .collect()
}

fn build_override_exports(
    explicit_env_overrides: &HashMap<String, String>,
    restore_even_when_absent: &[&str],
) -> (String, String) {
    let mut keys = explicit_env_overrides
        .keys()
        .map(String::as_str)
        .chain(restore_even_when_absent.iter().copied())
        .filter(|key| is_valid_shell_variable_name(key))
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();

    build_override_exports_for_keys("__CODEX_SNAPSHOT_OVERRIDE", &keys)
}

fn is_protected_snapshot_override(key: &str) -> bool {
    [
        CODEX_PERMISSION_PROFILE_ENV_VAR,
        CODEX_APPLY_PATCH_PRESERVE_LINE_ENDINGS_ENV_VAR,
        "OPENAI_FEDERATION_RULE_ID",
        "OPENAI_IDENTITY_TOKEN_FILE",
    ]
    .iter()
    .any(|protected| protected.eq_ignore_ascii_case(key))
}

fn build_proxy_env_exports() -> (String, String) {
    let mut keys = PROXY_ENV_KEYS
        .iter()
        .copied()
        .chain(CUSTOM_CA_ENV_KEYS)
        .filter(|key| is_valid_shell_variable_name(key))
        .collect::<Vec<_>>();
    keys.sort_unstable();
    keys.dedup();

    let (captures, restores) =
        build_override_exports_for_keys("__CODEX_SNAPSHOT_PROXY_OVERRIDE", &keys);
    let key = PROXY_ACTIVE_ENV_KEY;
    let proxy_blocks = (
        format!("{captures}\n__CODEX_SNAPSHOT_PROXY_ENV_SET=\"${{{key}+x}}\""),
        format!(
            "if [ -n \"$__CODEX_SNAPSHOT_PROXY_ENV_SET\" ] || [ -n \"${{{key}+x}}\" ]; then\n{restores}\nfi"
        ),
    );
    let git_blocks = build_codex_proxy_git_ssh_command_exports();
    (
        join_shell_blocks([proxy_blocks.0, git_blocks.0]),
        join_shell_blocks([proxy_blocks.1, git_blocks.1]),
    )
}

#[cfg(target_os = "macos")]
fn build_codex_proxy_git_ssh_command_exports() -> (String, String) {
    let key = PROXY_GIT_SSH_COMMAND_ENV_KEY;
    let marker_pattern = format!("{}\\ *", CODEX_PROXY_GIT_SSH_COMMAND_MARKER.trim_end());
    (
        format!(
            "__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_SET=\"${{{key}+x}}\"\n__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND=\"${{{key}-}}\"\ncase \"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND\" in\n  {marker_pattern}) __CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_LIVE_MARKED=1 ;;\n  *) __CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_LIVE_MARKED= ;;\nesac"
        ),
        format!(
            "case \"${{{key}-}}\" in\n  {marker_pattern}) __CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_AFTER_MARKED=1 ;;\n  *) __CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_AFTER_MARKED= ;;\nesac\nif [ -n \"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_LIVE_MARKED\" ]; then\n  if [ -z \"${{{key}+x}}\" ] || [ -n \"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_AFTER_MARKED\" ]; then\n    export {key}=\"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND\"\n  fi\nelif [ -n \"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_AFTER_MARKED\" ]; then\n  if [ -n \"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND_SET\" ]; then\n    export {key}=\"$__CODEX_SNAPSHOT_PROXY_GIT_SSH_COMMAND\"\n  else\n    unset {key}\n  fi\nfi"
        ),
    )
}

#[cfg(not(target_os = "macos"))]
fn build_codex_proxy_git_ssh_command_exports() -> (String, String) {
    (String::new(), String::new())
}

fn build_override_exports_for_keys(variable_prefix: &str, keys: &[&str]) -> (String, String) {
    if keys.is_empty() {
        return (String::new(), String::new());
    }

    let captures = keys
        .iter()
        .enumerate()
        .map(|(idx, key)| {
            let set_var = format!("{variable_prefix}_SET_{idx}");
            let value_var = format!("{variable_prefix}_{idx}");
            format!("{set_var}=\"${{{key}+x}}\"\n{value_var}=\"${{{key}-}}\"")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let restores = keys
        .iter()
        .enumerate()
        .map(|(idx, key)| {
            let set_var = format!("{variable_prefix}_SET_{idx}");
            let value_var = format!("{variable_prefix}_{idx}");
            format!(
                "if [ -n \"${{{set_var}}}\" ]; then export {key}=\"${{{value_var}}}\"; else unset {key}; fi"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    (captures, restores)
}

fn join_shell_blocks(blocks: impl IntoIterator<Item = String>) -> String {
    blocks
        .into_iter()
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn is_valid_shell_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return false;
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn shell_single_quote(input: &str) -> String {
    input.replace('\'', r#"'"'"'"#)
}

#[cfg(test)]
mod disable_powershell_profile_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn inserts_no_profile_for_elevated_windows_sandbox() {
        let command = vec![
            "powershell.exe".to_string(),
            "-Command".to_string(),
            "Write-Output ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::Elevated,
            /*proxy_enforced*/ false,
        );

        assert_eq!(
            rewritten,
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Write-Output ok".to_string(),
            ]
        );
    }

    #[test]
    fn inserts_no_profile_for_proxy_selected_elevated_windows_sandbox() {
        let command = vec![
            "powershell.exe".to_string(),
            "-Command".to_string(),
            "Write-Output ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::RestrictedToken,
            /*proxy_enforced*/ true,
        );

        assert_eq!(
            rewritten,
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Write-Output ok".to_string(),
            ]
        );
    }

    #[test]
    fn inserts_no_profile_before_encoded_command() {
        let command = vec![
            "powershell.exe".to_string(),
            "-EncodedCommand".to_string(),
            "VwByAGkAdABlAC0ATwB1AHQAcAB1AHQAIABvAGsA".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::Elevated,
            /*proxy_enforced*/ false,
        );

        assert_eq!(
            rewritten,
            vec![
                "powershell.exe".to_string(),
                "-NoProfile".to_string(),
                "-EncodedCommand".to_string(),
                "VwByAGkAdABlAC0ATwB1AHQAcAB1AHQAIABvAGsA".to_string(),
            ]
        );
    }

    #[test]
    fn preserves_existing_no_profile() {
        let command = vec![
            "pwsh.exe".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Write-Output ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::Elevated,
            /*proxy_enforced*/ false,
        );

        assert_eq!(rewritten, command);
    }

    #[test]
    fn leaves_legacy_restricted_token_backend_alone() {
        let command = vec![
            "powershell.exe".to_string(),
            "-Command".to_string(),
            "Write-Output ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::RestrictedToken,
            /*proxy_enforced*/ false,
        );

        assert_eq!(rewritten, command);
    }

    #[test]
    fn leaves_unsandboxed_attempts_alone() {
        let command = vec![
            "powershell.exe".to_string(),
            "-Command".to_string(),
            "Write-Output ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::PowerShell),
            SandboxType::None,
            WindowsSandboxLevel::Elevated,
            /*proxy_enforced*/ false,
        );

        assert_eq!(rewritten, command);
    }

    #[test]
    fn leaves_non_powershell_alone() {
        let command = vec![
            "/bin/bash".to_string(),
            "-lc".to_string(),
            "echo ok".to_string(),
        ];

        let rewritten = disable_powershell_profile_for_elevated_windows_sandbox(
            &command,
            Some(&ShellType::Bash),
            SandboxType::WindowsRestrictedToken,
            WindowsSandboxLevel::Elevated,
            /*proxy_enforced*/ false,
        );

        assert_eq!(rewritten, command);
    }
}

#[cfg(all(test, unix))]
#[path = "mod_tests.rs"]
mod tests;
