use crate::function_tool::FunctionCallError;
use crate::is_safe_command::is_known_safe_command;
use crate::maybe_emit_implicit_skill_invocation;
use crate::sandboxing::SandboxPermissions;
use crate::shell::Shell;
use crate::shell::get_shell_by_model_provided_path;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::apply_granted_turn_permissions;
use crate::tools::handlers::apply_patch::intercept_apply_patch;
use crate::tools::handlers::implicit_granted_permissions;
use crate::tools::handlers::normalize_and_validate_additional_permissions;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::parse_arguments_with_base_path;
use crate::tools::handlers::resolve_workdir_base_path;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::unified_exec::ExecCommandRequest;
use crate::unified_exec::MIN_EMPTY_YIELD_TIME_MS;
use crate::unified_exec::UNIFIED_EXEC_OUTPUT_MAX_BYTES;
use crate::unified_exec::UnifiedExecContext;
use crate::unified_exec::UnifiedExecProcessManager;
use crate::unified_exec::WriteStdinRequest;
use codex_features::Feature;
use codex_otel::SessionTelemetry;
use codex_otel::TOOL_CALL_UNIFIED_EXEC_METRIC;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TerminalInteractionEvent;
use codex_tools::UnifiedExecShellMode;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_output_truncation::approx_token_count;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub struct UnifiedExecHandler;

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
    additional_permissions: Option<PermissionProfile>,
    #[serde(default)]
    justification: Option<String>,
    #[serde(default)]
    prefix_rule: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct WriteStdinArgs {
    // The model is trained on `session_id`.
    session_id: i32,
    #[serde(default)]
    chars: String,
    #[serde(default = "default_write_stdin_yield_time_ms")]
    yield_time_ms: u64,
    #[serde(default)]
    max_output_tokens: Option<usize>,
    #[serde(default)]
    wait_until_terminal: bool,
    #[serde(default)]
    max_wait_ms: Option<u64>,
    #[serde(default)]
    heartbeat_interval_ms: Option<u64>,
}

fn default_exec_yield_time_ms() -> u64 {
    10_000
}

fn default_write_stdin_yield_time_ms() -> u64 {
    250
}

const DEFAULT_WAIT_HEARTBEAT_INTERVAL_MS: u64 = 30_000;

fn default_tty() -> bool {
    false
}

fn resolve_wait_bounds(
    manager: &UnifiedExecProcessManager,
    max_wait_ms: Option<u64>,
    heartbeat_interval_ms: Option<u64>,
) -> (u64, u64) {
    let configured_max_wait_ms = manager.max_write_stdin_yield_time_ms();
    let effective_max_wait_ms = max_wait_ms
        .unwrap_or(configured_max_wait_ms)
        .clamp(MIN_EMPTY_YIELD_TIME_MS, configured_max_wait_ms);
    let effective_heartbeat_interval_ms = heartbeat_interval_ms
        .unwrap_or(DEFAULT_WAIT_HEARTBEAT_INTERVAL_MS)
        .clamp(MIN_EMPTY_YIELD_TIME_MS, effective_max_wait_ms);

    (effective_max_wait_ms, effective_heartbeat_interval_ms)
}

struct WaitForTerminalCompletionArgs {
    process_id: i32,
    max_output_tokens: Option<usize>,
    max_wait_ms: u64,
    heartbeat_interval_ms: u64,
    initial_response: Option<ExecCommandToolOutput>,
}

async fn wait_for_terminal_completion(
    manager: &UnifiedExecProcessManager,
    session: &crate::codex::Session,
    turn: &crate::codex::TurnContext,
    args: WaitForTerminalCompletionArgs,
) -> Result<ExecCommandToolOutput, FunctionCallError> {
    let WaitForTerminalCompletionArgs {
        process_id,
        max_output_tokens,
        max_wait_ms,
        heartbeat_interval_ms,
        initial_response,
    } = args;
    let wait_started_at = Instant::now();
    let mut last_heartbeat_at = wait_started_at;
    let mut collected = initial_response
        .as_ref()
        .map_or_else(Vec::new, |response| response.raw_output.clone());
    if collected.len() > UNIFIED_EXEC_OUTPUT_MAX_BYTES {
        let overflow = collected.len() - UNIFIED_EXEC_OUTPUT_MAX_BYTES;
        collected.drain(..overflow);
    }
    let mut last_response = initial_response;
    let mut timed_out = false;

    loop {
        let elapsed_ms = wait_started_at.elapsed().as_millis() as u64;
        if elapsed_ms >= max_wait_ms {
            timed_out = true;
            break;
        }

        let remaining_ms = max_wait_ms.saturating_sub(elapsed_ms);
        let poll_yield_ms = remaining_ms
            .min(heartbeat_interval_ms)
            .max(MIN_EMPTY_YIELD_TIME_MS);
        let poll_response = manager
            .write_stdin(WriteStdinRequest {
                process_id,
                input: "",
                yield_time_ms: poll_yield_ms,
                max_output_tokens,
            })
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!("write_stdin failed: {err}"))
            })?;

        if !poll_response.raw_output.is_empty() {
            collected.extend_from_slice(&poll_response.raw_output);
            if collected.len() > UNIFIED_EXEC_OUTPUT_MAX_BYTES {
                let overflow = collected.len() - UNIFIED_EXEC_OUTPUT_MAX_BYTES;
                collected.drain(..overflow);
            }
        }

        let still_running = poll_response.process_id.is_some() && poll_response.exit_code.is_none();
        if still_running
            && (last_heartbeat_at.elapsed().as_millis() as u64) >= heartbeat_interval_ms
        {
            let running_for_secs = wait_started_at.elapsed().as_secs();
            session
                .notify_background_event(
                    turn,
                    format!(
                        "Process {process_id} still running after {running_for_secs}s (captured {} bytes).",
                        collected.len()
                    ),
                )
                .await;
            last_heartbeat_at = Instant::now();
        }

        last_response = Some(poll_response);
        if !still_running {
            break;
        }
    }

    let mut response = last_response.ok_or_else(|| {
        FunctionCallError::RespondToModel(
            "write_stdin failed: no process output captured while waiting".to_string(),
        )
    })?;

    response.raw_output = collected;
    response.max_output_tokens = max_output_tokens;
    response.wall_time = wait_started_at.elapsed();

    if timed_out && response.process_id.is_some() && response.exit_code.is_none() {
        let waited_secs = response.wall_time.as_secs();
        let timeout_note = format!(
            "Wait timed out after {waited_secs}s; process is still running. Re-issue exec_command or write_stdin with wait_until_terminal=true to continue waiting."
        );
        let current_text = String::from_utf8_lossy(&response.raw_output).to_string();
        if current_text.is_empty() {
            response.raw_output = timeout_note.into_bytes();
        } else {
            response.raw_output = format!("{current_text}\n\n{timeout_note}").into_bytes();
        }
    }
    let final_text = String::from_utf8_lossy(&response.raw_output).to_string();
    response.original_token_count = Some(approx_token_count(&final_text));

    Ok(response)
}

impl ToolHandler for UnifiedExecHandler {
    type Output = ExecCommandToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            tracing::error!(
                "This should never happen, invocation payload is wrong: {:?}",
                invocation.payload
            );
            return true;
        };

        let Ok(params) = parse_arguments::<ExecCommandArgs>(arguments) else {
            return true;
        };
        let command = match get_command(
            &params,
            invocation.session.user_shell(),
            &invocation.turn.tools_config.unified_exec_shell_mode,
            invocation.turn.tools_config.allow_login_shell,
        ) {
            Ok(command) => command,
            Err(_) => return true,
        };
        !is_known_safe_command(&command)
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        if invocation.tool_name != "exec_command".into() {
            return None;
        }

        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        parse_arguments::<ExecCommandArgs>(arguments)
            .ok()
            .map(|args| PreToolUsePayload { command: args.cmd })
    }

    fn post_tool_use_payload(
        &self,
        call_id: &str,
        payload: &ToolPayload,
        result: &dyn ToolOutput,
    ) -> Option<PostToolUsePayload> {
        let ToolPayload::Function { arguments } = payload else {
            return None;
        };

        let args = parse_arguments::<ExecCommandArgs>(arguments).ok()?;
        if args.tty {
            return None;
        }

        let tool_response = result.post_tool_use_response(call_id, payload)?;
        Some(PostToolUsePayload {
            command: args.cmd,
            tool_response,
        })
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            tracker,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "unified_exec handler received unsupported payload".to_string(),
                ));
            }
        };

        let manager: &UnifiedExecProcessManager = &session.services.unified_exec_manager;
        let context = UnifiedExecContext::new(session.clone(), turn.clone(), call_id.clone());

        let response = match tool_name.name.as_str() {
            "exec_command" => {
                let cwd = resolve_workdir_base_path(&arguments, &context.turn.cwd)?;
                let args: ExecCommandArgs = parse_arguments_with_base_path(&arguments, &cwd)?;
                let workdir = context.turn.resolve_path(args.workdir.clone());
                maybe_emit_implicit_skill_invocation(
                    session.as_ref(),
                    context.turn.as_ref(),
                    &args.cmd,
                    &workdir,
                )
                .await;
                let process_id = manager.allocate_process_id().await;
                let command = get_command(
                    &args,
                    session.user_shell(),
                    &turn.tools_config.unified_exec_shell_mode,
                    turn.tools_config.allow_login_shell,
                )
                .map_err(FunctionCallError::RespondToModel)?;
                let command_for_display = codex_shell_command::parse_command::shlex_join(&command);

                let ExecCommandArgs {
                    workdir,
                    tty,
                    yield_time_ms,
                    max_output_tokens,
                    wait_until_terminal,
                    max_wait_ms,
                    heartbeat_interval_ms,
                    sandbox_permissions,
                    additional_permissions,
                    justification,
                    prefix_rule,
                    ..
                } = args;

                let exec_permission_approvals_enabled =
                    session.features().enabled(Feature::ExecPermissionApprovals);
                let requested_additional_permissions = additional_permissions.clone();
                let effective_additional_permissions = apply_granted_turn_permissions(
                    context.session.as_ref(),
                    sandbox_permissions,
                    additional_permissions,
                )
                .await;
                let additional_permissions_allowed = exec_permission_approvals_enabled
                    || (session.features().enabled(Feature::RequestPermissionsTool)
                        && effective_additional_permissions.permissions_preapproved);

                // Sticky turn permissions have already been approved, so they should
                // continue through the normal exec approval flow for the command.
                if effective_additional_permissions
                    .sandbox_permissions
                    .requests_sandbox_override()
                    && !effective_additional_permissions.permissions_preapproved
                    && !matches!(
                        context.turn.approval_policy.value(),
                        codex_protocol::protocol::AskForApproval::OnRequest
                    )
                {
                    let approval_policy = context.turn.approval_policy.value();
                    manager.release_process_id(process_id).await;
                    return Err(FunctionCallError::RespondToModel(format!(
                        "approval policy is {approval_policy:?}; reject command — you cannot ask for escalated permissions if the approval policy is {approval_policy:?}"
                    )));
                }

                let workdir = workdir.filter(|value| !value.is_empty());

                let workdir = workdir.map(|dir| {
                    AbsolutePathBuf::try_from(context.turn.resolve_path(Some(dir)))
                        .expect("exec_command workdir must resolve to an absolute path")
                });
                let cwd = workdir.clone().unwrap_or(cwd);
                let normalized_additional_permissions = match implicit_granted_permissions(
                    sandbox_permissions,
                    requested_additional_permissions.as_ref(),
                    &effective_additional_permissions,
                )
                .map_or_else(
                    || {
                        normalize_and_validate_additional_permissions(
                            additional_permissions_allowed,
                            context.turn.approval_policy.value(),
                            effective_additional_permissions.sandbox_permissions,
                            effective_additional_permissions.additional_permissions,
                            effective_additional_permissions.permissions_preapproved,
                            &cwd,
                        )
                    },
                    |permissions| Ok(Some(permissions)),
                ) {
                    Ok(normalized) => normalized,
                    Err(err) => {
                        manager.release_process_id(process_id).await;
                        return Err(FunctionCallError::RespondToModel(err));
                    }
                };

                let exec_fs = context.turn.environment.get_filesystem();
                if let Some(output) = intercept_apply_patch(
                    &command,
                    &cwd,
                    exec_fs.as_ref(),
                    Some(yield_time_ms),
                    context.session.clone(),
                    context.turn.clone(),
                    Some(&tracker),
                    &context.call_id,
                    tool_name.name.as_str(),
                )
                .await?
                {
                    manager.release_process_id(process_id).await;
                    return Ok(ExecCommandToolOutput {
                        event_call_id: String::new(),
                        chunk_id: String::new(),
                        wall_time: std::time::Duration::ZERO,
                        raw_output: output.into_text().into_bytes(),
                        max_output_tokens: None,
                        process_id: None,
                        exit_code: None,
                        original_token_count: None,
                        session_command: None,
                    });
                }

                emit_unified_exec_tty_metric(&turn.session_telemetry, tty);
                let response = manager
                    .exec_command(
                        ExecCommandRequest {
                            command,
                            process_id,
                            yield_time_ms,
                            max_output_tokens,
                            workdir,
                            network: context.turn.network.clone(),
                            tty,
                            sandbox_permissions: effective_additional_permissions
                                .sandbox_permissions,
                            additional_permissions: normalized_additional_permissions,
                            additional_permissions_preapproved: effective_additional_permissions
                                .permissions_preapproved,
                            justification,
                            prefix_rule,
                        },
                        &context,
                    )
                    .await
                    .map_err(|err| {
                        FunctionCallError::RespondToModel(format!(
                            "exec_command failed for `{command_for_display}`: {err:?}"
                        ))
                    })?;

                if wait_until_terminal {
                    match response.process_id {
                        Some(active_process_id) if response.exit_code.is_none() => {
                            let (effective_max_wait_ms, effective_heartbeat_interval_ms) =
                                resolve_wait_bounds(manager, max_wait_ms, heartbeat_interval_ms);
                            wait_for_terminal_completion(
                                manager,
                                session.as_ref(),
                                turn.as_ref(),
                                WaitForTerminalCompletionArgs {
                                    process_id: active_process_id,
                                    max_output_tokens,
                                    max_wait_ms: effective_max_wait_ms,
                                    heartbeat_interval_ms: effective_heartbeat_interval_ms,
                                    initial_response: Some(response),
                                },
                            )
                            .await?
                        }
                        _ => response,
                    }
                } else {
                    response
                }
            }
            "write_stdin" => {
                let args: WriteStdinArgs = parse_arguments(&arguments)?;
                if args.wait_until_terminal && !args.chars.is_empty() {
                    return Err(FunctionCallError::RespondToModel(
                        "wait_until_terminal=true requires chars to be empty; send stdin in one call, then wait in a separate call.".to_string(),
                    ));
                }

                let response = if args.wait_until_terminal {
                    let (effective_max_wait_ms, effective_heartbeat_interval_ms) =
                        resolve_wait_bounds(manager, args.max_wait_ms, args.heartbeat_interval_ms);
                    wait_for_terminal_completion(
                        manager,
                        session.as_ref(),
                        turn.as_ref(),
                        WaitForTerminalCompletionArgs {
                            process_id: args.session_id,
                            max_output_tokens: args.max_output_tokens,
                            max_wait_ms: effective_max_wait_ms,
                            heartbeat_interval_ms: effective_heartbeat_interval_ms,
                            initial_response: None,
                        },
                    )
                    .await?
                } else {
                    manager
                        .write_stdin(WriteStdinRequest {
                            process_id: args.session_id,
                            input: &args.chars,
                            yield_time_ms: args.yield_time_ms,
                            max_output_tokens: args.max_output_tokens,
                        })
                        .await
                        .map_err(|err| {
                            FunctionCallError::RespondToModel(format!("write_stdin failed: {err}"))
                        })?
                };

                let interaction = TerminalInteractionEvent {
                    call_id: response.event_call_id.clone(),
                    process_id: args.session_id.to_string(),
                    stdin: args.chars.clone(),
                };
                session
                    .send_event(turn.as_ref(), EventMsg::TerminalInteraction(interaction))
                    .await;

                response
            }
            other => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "unsupported unified exec function {other}"
                )));
            }
        };

        Ok(response)
    }
}

fn emit_unified_exec_tty_metric(session_telemetry: &SessionTelemetry, tty: bool) {
    session_telemetry.counter(
        TOOL_CALL_UNIFIED_EXEC_METRIC,
        /*inc*/ 1,
        &[("tty", if tty { "true" } else { "false" })],
    );
}

pub(crate) fn get_command(
    args: &ExecCommandArgs,
    session_shell: Arc<Shell>,
    shell_mode: &UnifiedExecShellMode,
    allow_login_shell: bool,
) -> Result<Vec<String>, String> {
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
            Ok(shell.derive_exec_args(&args.cmd, use_login_shell))
        }
        UnifiedExecShellMode::ZshFork(zsh_fork_config) => Ok(vec![
            zsh_fork_config.shell_zsh_path.to_string_lossy().to_string(),
            if use_login_shell { "-lc" } else { "-c" }.to_string(),
            args.cmd.clone(),
        ]),
    }
}

#[cfg(test)]
#[path = "unified_exec_tests.rs"]
mod tests;
