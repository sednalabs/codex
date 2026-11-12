use crate::function_tool::FunctionCallError;
use crate::sandboxing::SandboxPermissions;
use crate::session::turn_context::TurnEnvironment;
use crate::shell::Shell;
use crate::shell::ShellInvocation;
use crate::shell::get_shell_by_model_provided_path;
use crate::tools::context::ToolEnvironments;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::resolve_tool_environment;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::PostToolUsePayload;
use crate::unified_exec::MIN_YIELD_TIME_MS;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecError;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::WriteStdinRequest;
use crate::unified_exec::output::CapturedStream;
use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use codex_utils_head_tail_buffer::HeadTailBuffer;
use codex_protocol::models::AdditionalPermissionProfile;
use codex_utils_path_uri::PathUri;
use futures::future::BoxFuture;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

mod exec_command;
pub(crate) mod preparation;
mod write_stdin;

pub use exec_command::ExecCommandHandler;
pub(crate) use exec_command::ExecCommandHandlerOptions;
pub use write_stdin::WriteStdinHandler;

/// Terminal tools expose the unified-exec process ID as `session_id`.
pub(crate) fn terminal_session_for_input(
    invocation: &ToolInvocation,
) -> BoxFuture<'_, Result<ToolEnvironments, FunctionCallError>> {
    #[derive(Deserialize)]
    struct TerminalSessionArgs {
        session_id: i32,
    }

    Box::pin(async move {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return Err(FunctionCallError::Fatal(
                "expected function arguments for terminal session".to_string(),
            ));
        };
        let args: TerminalSessionArgs = parse_arguments(arguments)?;
        resolve_tool_environment(
            &invocation.step_context.environments,
            /*environment_id*/ None,
        )?;
        invocation
            .session
            .services
            .unified_exec_manager
            .resolve_terminal(args.session_id)
            .await
            .map(ToolEnvironments::Terminal)
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "{} failed: {err}",
                    invocation.tool_name.name
                ))
            })
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct ExecCommandArgs {
    pub(crate) cmd: String,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    login: Option<bool>,
    #[serde(default = "default_tty")]
    tty: bool,
    #[serde(default = "default_exec_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    wait_until_terminal: bool,
    #[serde(default)]
    max_wait_ms: Option<u64>,
    #[serde(default)]
    heartbeat_interval_ms: Option<u64>,
    #[serde(default)]
    sandbox_permissions: Option<SandboxPermissions>,
    #[serde(default)]
    additional_permissions: Option<AdditionalPermissionProfile>,
    #[serde(default)]
    justification: Option<String>,
    #[serde(default)]
    prefix_rule: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct ExecCommandWorkdirArgs {
    // Keep this raw until after environment selection; relative paths must be
    // resolved against the selected environment cwd, not the process cwd.
    #[serde(default)]
    workdir: Option<String>,
}

impl ExecCommandWorkdirArgs {
    /// Share cwd resolution between hooks and execution after environment selection.
    fn resolve_cwd(&self, environment: &TurnEnvironment) -> Result<PathUri, FunctionCallError> {
        self.workdir
            .as_deref()
            .filter(|workdir| !workdir.is_empty())
            .map_or_else(
                || Ok(environment.cwd().clone()),
                |workdir| environment.cwd().join(workdir),
            )
            .map_err(|err| FunctionCallError::RespondToModel(err.to_string()))
    }
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

const MAX_TERMINAL_WAIT_WINDOW_MS: u64 = 2 * 60 * 60 * 1_000;
const APPROX_BYTES_PER_TOKEN: usize = 4;

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub(crate) struct TerminalWaitArgs {
    #[serde(default)]
    pub(crate) wait_until_terminal: bool,
    #[serde(default)]
    pub(crate) max_wait_ms: Option<u64>,
    #[serde(default)]
    pub(crate) heartbeat_interval_ms: Option<u64>,
}

fn resolve_terminal_wait_window_ms(args: TerminalWaitArgs, fallback_ms: u64) -> u64 {
    args.heartbeat_interval_ms
        .or(args.max_wait_ms)
        .unwrap_or(fallback_ms)
        .max(MIN_YIELD_TIME_MS)
        .min(MAX_TERMINAL_WAIT_WINDOW_MS)
}

/// Keep repeated internal polls bounded while leaving the process and its session resumable.
struct TerminalWaitOutput {
    bytes: HeadTailBuffer<UNIFIED_EXEC_OUTPUT_MAX_BYTES>,
    original_tokens: usize,
    omitted_bytes: usize,
}

impl TerminalWaitOutput {
    fn new() -> Self {
        Self {
            bytes: HeadTailBuffer::default(),
            original_tokens: 0,
            omitted_bytes: 0,
        }
    }

    fn record_response(&mut self, response: &mut crate::tools::context::ExecCommandToolOutput) {
        let captured = std::mem::replace(
            &mut response.raw_output,
            CapturedStream {
                text: String::new(),
                omitted_bytes: 0,
            },
        );
        self.original_tokens = self.original_tokens.saturating_add(
            response
                .original_token_count
                .unwrap_or_else(|| captured.text.len().div_ceil(APPROX_BYTES_PER_TOKEN)),
        );
        self.omitted_bytes = self.omitted_bytes.saturating_add(captured.omitted_bytes);
        self.bytes.push_chunk(captured.text.into_bytes());
    }

    fn finish(self, response: &mut crate::tools::context::ExecCommandToolOutput) {
        let omitted_bytes = self
            .omitted_bytes
            .saturating_add(self.bytes.omitted_bytes());
        response.raw_output = CapturedStream {
            text: String::from_utf8_lossy(&self.bytes.to_bytes()).into_owned(),
            omitted_bytes,
        };
        response.original_token_count = Some(self.original_tokens);
    }
}

pub(crate) async fn complete_terminal_wait(
    manager: &UnifiedExecProcessManager,
    context: &UnifiedExecContext,
    initial_response: crate::tools::context::ExecCommandToolOutput,
    args: TerminalWaitArgs,
    fallback_yield_time_ms: u64,
) -> Result<crate::tools::context::ExecCommandToolOutput, UnifiedExecError> {
    if !args.wait_until_terminal || initial_response.process_id.is_none() {
        return Ok(initial_response);
    }

    let wait_window_ms = resolve_terminal_wait_window_ms(args, fallback_yield_time_ms);
    let mut output = TerminalWaitOutput::new();
    let mut response = initial_response;
    let mut wall_time = response.wall_time;
    output.record_response(&mut response);

    while let Some(process_id) = response.process_id {
        let terminal = manager.resolve_terminal(process_id).await?;
        let next = tokio::select! {
            _ = context.cancellation_token.cancelled() => break,
            result = manager.write_stdin(
                context,
                WriteStdinRequest {
                    terminal: &terminal,
                    input: "",
                    yield_time_ms: wait_window_ms,
                    max_output_tokens: response.max_output_tokens,
                    truncation_policy: response.truncation_policy,
                    interaction_event: None,
                },
            ) => result,
        };
        response = match next {
            Ok(response) => response,
            Err(_) if context.cancellation_token.is_cancelled() => break,
            Err(err) => return Err(err),
        };
        wall_time += response.wall_time;
        output.record_response(&mut response);
    }

    response.wall_time = wall_time;
    output.finish(&mut response);
    Ok(response)
}

#[derive(Debug)]
pub(crate) struct ResolvedCommand {
    pub(crate) command: Vec<String>,
    pub(crate) shell: ShellInvocation,
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
        tool_cwd: result.post_tool_use_cwd(),
        tool_name: HookToolName::bash(),
        tool_use_id,
        tool_input,
        tool_response,
    })
}

pub(crate) fn get_command(
    args: &ExecCommandArgs,
    session_shell: Arc<Shell>,
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

    let shell = args
        .shell
        .as_ref()
        .map(|shell_str| get_shell_by_model_provided_path(&PathBuf::from(shell_str)))
        .unwrap_or_else(|| session_shell.as_ref().clone());
    Ok(ResolvedCommand {
        command: shell.derive_exec_args(&args.cmd, use_login_shell),
        shell: ShellInvocation {
            shell,
            use_login_shell,
        },
    })
}

#[cfg(test)]
#[path = "unified_exec_tests.rs"]
mod tests;
