use crate::sandboxing::SandboxPermissions;
use crate::shell::Shell;
use crate::shell::ShellType;
use crate::shell::get_shell_by_model_provided_path;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::tool_runtime_capabilities::UnifiedExecBlockingWaitCapability;
use crate::tools::tool_runtime_capabilities::registered_tool_runtime_capabilities;
use crate::unified_exec::MIN_YIELD_TIME_MS;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::WriteStdinRequest;
use crate::unified_exec::resolve_max_tokens;
use codex_exec_server::Environment;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_tools::UnifiedExecShellMode;
use codex_utils_output_truncation::TruncationPolicy;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const APPROX_BYTES_PER_TOKEN: usize = 4;

#[cfg(test)]
use crate::tools::handlers::parse_arguments;

mod exec_command;
mod write_stdin;

pub use exec_command::ExecCommandHandler;
pub(crate) use exec_command::ExecCommandHandlerOptions;
pub use write_stdin::WriteStdinHandler;

#[derive(Debug, Deserialize)]
pub(crate) struct ExecCommandArgs {
    cmd: String,
    #[serde(default)]
    pub(crate) workdir: Option<String>,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    login: Option<bool>,
    #[serde(default = "default_tty")]
    tty: bool,
    #[serde(default = "default_exec_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    wait_until_terminal: bool,
    #[serde(default)]
    max_wait_ms: Option<u64>,
    #[serde(default)]
    heartbeat_interval_ms: Option<u64>,
    #[serde(default)]
    sandbox_permissions: SandboxPermissions,
    #[serde(default)]
    additional_permissions: Option<AdditionalPermissionProfile>,
    #[serde(default)]
    justification: Option<String>,
    #[serde(default)]
    prefix_rule: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
struct TerminalWaitArgs {
    #[serde(default)]
    wait_until_terminal: bool,
    #[serde(default)]
    max_wait_ms: Option<u64>,
    #[serde(default)]
    heartbeat_interval_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ExecCommandEnvironmentArgs {
    #[serde(default)]
    environment_id: Option<String>,
    // Keep this raw until after environment selection; relative paths must be
    // resolved against the selected environment cwd, not the process cwd.
    #[serde(default)]
    workdir: Option<String>,
}

fn default_exec_yield_time_ms() -> u64 {
    10_000
}

fn default_write_stdin_yield_time_ms() -> u64 {
    250
}

fn default_tty() -> bool {
    false
}

fn unified_exec_blocking_wait_capability() -> Option<UnifiedExecBlockingWaitCapability> {
    registered_tool_runtime_capabilities().unified_exec_blocking_waits
}

fn default_wait_budget_ms(capability: UnifiedExecBlockingWaitCapability) -> u64 {
    capability.max_terminal_wait_ms
}

fn resolve_wait_window_ms(
    max_wait_ms: Option<u64>,
    heartbeat_interval_ms: Option<u64>,
    fallback_ms: u64,
    capability: UnifiedExecBlockingWaitCapability,
) -> u64 {
    heartbeat_interval_ms
        .or(max_wait_ms)
        .unwrap_or(fallback_ms)
        .max(MIN_YIELD_TIME_MS)
        .min(default_wait_budget_ms(capability))
}

async fn complete_terminal_wait(
    manager: &UnifiedExecProcessManager,
    initial_response: ExecCommandToolOutput,
    terminal_wait: TerminalWaitArgs,
    capability: UnifiedExecBlockingWaitCapability,
    fallback_yield_time_ms: u64,
    cancellation_token: &CancellationToken,
) -> Result<ExecCommandToolOutput, UnifiedExecError> {
    if initial_response.process_id.is_none() {
        return Ok(initial_response);
    }

    let wait_window_ms = resolve_wait_window_ms(
        terminal_wait.max_wait_ms,
        terminal_wait.heartbeat_interval_ms,
        fallback_yield_time_ms,
        capability,
    );
    let max_output_tokens = initial_response.max_output_tokens;
    let mut response = initial_response;
    let mut raw_output = std::mem::take(&mut response.raw_output);
    let mut wall_time = response.wall_time;

    while let Some(process_id) = response.process_id {
        tokio::select! {
            _ = cancellation_token.cancelled() => break,
            result = manager.write_stdin(WriteStdinRequest {
                process_id,
                input: "",
                yield_time_ms: wait_window_ms,
                empty_input_min_yield_time_ms: MIN_YIELD_TIME_MS,
                max_output_tokens,
                truncation_policy: response.truncation_policy,
            }) => {
                response = result?;
            }
        }
        wall_time += response.wall_time;
        raw_output.extend_from_slice(&response.raw_output);
    }

    response.wall_time = wall_time;
    response.raw_output = raw_output;
    let output_text = String::from_utf8_lossy(&response.raw_output);
    response.original_token_count = Some(approx_token_count(&output_text));
    Ok(response)
}

fn approx_token_count(text: &str) -> usize {
    text.len().div_ceil(APPROX_BYTES_PER_TOKEN)
}

fn effective_max_output_tokens(
    max_output_tokens: Option<usize>,
    truncation_policy: TruncationPolicy,
) -> usize {
    resolve_max_tokens(max_output_tokens).min(truncation_policy.token_budget())
}

#[derive(Debug)]
pub(crate) struct ResolvedCommand {
    pub(crate) command: Vec<String>,
    pub(crate) shell_type: ShellType,
}

fn post_unified_exec_tool_use_payload(
    invocation: &ToolInvocation,
    result: &dyn ToolOutput,
) -> Option<PostToolUsePayload> {
    let ToolPayload::Function { .. } = &invocation.payload else {
        return None;
    };

    let tool_input = result.post_tool_use_input(&invocation.payload)?;
    let tool_use_id = result.post_tool_use_id(&invocation.call_id);
    let tool_response = result.post_tool_use_response(&tool_use_id, &invocation.payload)?;
    Some(PostToolUsePayload {
        tool_name: HookToolName::bash(),
        tool_use_id,
        tool_input,
        tool_response,
    })
}

pub(crate) fn get_command(
    args: &ExecCommandArgs,
    session_shell: Arc<Shell>,
    shell_mode: &UnifiedExecShellMode,
    allow_login_shell: bool,
) -> Result<ResolvedCommand, String> {
    let use_login_shell = match args.login {
        Some(true) if !allow_login_shell => {
            return Err(
                "login shell is disabled by config; omit `login` or set it to false.".to_string(),
            );
        }
        Some(use_login_shell) => use_login_shell,
        None => allow_login_shell,
    };

    match shell_mode {
        UnifiedExecShellMode::Direct => {
            let model_shell = args.shell.as_ref().map(|shell_str| {
                let mut shell = get_shell_by_model_provided_path(&PathBuf::from(shell_str));
                shell.shell_snapshot = crate::shell::empty_shell_snapshot_receiver();
                shell
            });
            let shell = model_shell.as_ref().unwrap_or(session_shell.as_ref());
            Ok(ResolvedCommand {
                command: shell.derive_exec_args(&args.cmd, use_login_shell),
                shell_type: shell.shell_type,
            })
        }
        UnifiedExecShellMode::ZshFork(zsh_fork_config) => {
            if args.shell.is_some() {
                return Err(
                    "`shell` is not supported for local zsh-fork exec; omit `shell` to use zsh-fork, or target a remote environment where `shell` is supported.".to_string(),
                );
            }

            Ok(ResolvedCommand {
                command: vec![
                    zsh_fork_config.shell_zsh_path.to_string_lossy().to_string(),
                    if use_login_shell { "-lc" } else { "-c" }.to_string(),
                    args.cmd.clone(),
                ],
                shell_type: ShellType::Zsh,
            })
        }
    }
}

pub(crate) fn shell_mode_for_environment(
    turn_shell_mode: &UnifiedExecShellMode,
    environment: &Environment,
) -> UnifiedExecShellMode {
    if environment.is_remote() {
        UnifiedExecShellMode::Direct
    } else {
        turn_shell_mode.clone()
    }
}

#[cfg(test)]
#[path = "unified_exec_tests.rs"]
mod tests;
