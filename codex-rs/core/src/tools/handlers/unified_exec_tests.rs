use super::*;
use crate::function_tool::FunctionCallError;
use crate::shell::default_user_shell;
use crate::tools::handlers::parse_arguments_with_base_path;
use crate::tools::handlers::resolve_workdir_base_path;
use codex_protocol::models::FileSystemPermissions;
use codex_protocol::models::PermissionProfile;
use codex_tools::UnifiedExecShellMode;
use codex_tools::ZshForkConfig;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::PathExt;
use core_test_support::skip_if_sandbox;
use pretty_assertions::assert_eq;
use std::fs;
use std::sync::Arc;
use tempfile::tempdir;

use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::ToolHandler;
use crate::turn_diff_tracker::TurnDiffTracker;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

fn invocation(
    session: Arc<crate::session::session::Session>,
    turn: Arc<TurnContext>,
    tool_name: &str,
    payload: ToolPayload,
) -> ToolInvocation {
    ToolInvocation {
        session,
        turn,
        cancellation_token: CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "call-unified-exec-test".to_string(),
        tool_name: codex_tools::ToolName::plain(tool_name),
        payload,
    }
}

fn function_payload(args: serde_json::Value) -> ToolPayload {
    ToolPayload::Function {
        arguments: args.to_string(),
    }
}

async fn run_unified_exec(
    session: Arc<crate::session::session::Session>,
    turn: Arc<TurnContext>,
    tool_name: &str,
    args: serde_json::Value,
) -> Result<ExecCommandToolOutput, FunctionCallError> {
    UnifiedExecHandler
        .handle(invocation(session, turn, tool_name, function_payload(args)))
        .await
}

async fn invocation_for_payload(
    tool_name: &str,
    call_id: &str,
    payload: ToolPayload,
) -> ToolInvocation {
    let (session, turn) = make_session_and_context().await;
    ToolInvocation {
        session: session.into(),
        turn: turn.into(),
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
        call_id: call_id.to_string(),
        tool_name: codex_tools::ToolName::plain(tool_name),
        source: ToolCallSource::Direct,
        payload,
    }
}

#[test]
fn test_get_command_uses_default_shell_when_unspecified() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello"}"#;

    let args: ExecCommandArgs = parse_arguments(json)?;

    assert!(args.shell.is_none());

    let command = get_command(
        &args,
        Arc::new(default_user_shell()),
        &UnifiedExecShellMode::Direct,
        /*allow_login_shell*/ true,
    )
    .map_err(anyhow::Error::msg)?;

    assert_eq!(command.len(), 3);
    assert_eq!(command[2], "echo hello");
    Ok(())
}

#[test]
fn test_get_command_respects_explicit_bash_shell() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello", "shell": "/bin/bash"}"#;

    let args: ExecCommandArgs = parse_arguments(json)?;

    assert_eq!(args.shell.as_deref(), Some("/bin/bash"));

    let command = get_command(
        &args,
        Arc::new(default_user_shell()),
        &UnifiedExecShellMode::Direct,
        /*allow_login_shell*/ true,
    )
    .map_err(anyhow::Error::msg)?;

    assert_eq!(command.last(), Some(&"echo hello".to_string()));
    if command
        .iter()
        .any(|arg| arg.eq_ignore_ascii_case("-Command"))
    {
        assert!(command.contains(&"-NoProfile".to_string()));
    }
    Ok(())
}

#[test]
fn test_get_command_respects_explicit_powershell_shell() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello", "shell": "powershell"}"#;

    let args: ExecCommandArgs = parse_arguments(json)?;

    assert_eq!(args.shell.as_deref(), Some("powershell"));

    let command = get_command(
        &args,
        Arc::new(default_user_shell()),
        &UnifiedExecShellMode::Direct,
        /*allow_login_shell*/ true,
    )
    .map_err(anyhow::Error::msg)?;

    assert_eq!(command[2], "echo hello");
    Ok(())
}

#[test]
fn test_get_command_respects_explicit_cmd_shell() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello", "shell": "cmd"}"#;

    let args: ExecCommandArgs = parse_arguments(json)?;

    assert_eq!(args.shell.as_deref(), Some("cmd"));

    let command = get_command(
        &args,
        Arc::new(default_user_shell()),
        &UnifiedExecShellMode::Direct,
        /*allow_login_shell*/ true,
    )
    .map_err(anyhow::Error::msg)?;

    assert_eq!(command[2], "echo hello");
    Ok(())
}

#[test]
fn test_get_command_rejects_explicit_login_when_disallowed() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello", "login": true}"#;

    let args: ExecCommandArgs = parse_arguments(json)?;
    let err = get_command(
        &args,
        Arc::new(default_user_shell()),
        &UnifiedExecShellMode::Direct,
        /*allow_login_shell*/ false,
    )
    .expect_err("explicit login should be rejected");

    assert!(
        err.contains("login shell is disabled by config"),
        "unexpected error: {err}"
    );
    Ok(())
}

#[test]
fn test_get_command_ignores_explicit_shell_in_zsh_fork_mode() -> anyhow::Result<()> {
    let json = r#"{"cmd": "echo hello", "shell": "/bin/bash"}"#;
    let args: ExecCommandArgs = parse_arguments(json)?;
    let shell_zsh_path = AbsolutePathBuf::from_absolute_path(if cfg!(windows) {
        r"C:\opt\codex\zsh"
    } else {
        "/opt/codex/zsh"
    })?;
    let shell_mode = UnifiedExecShellMode::ZshFork(ZshForkConfig {
        shell_zsh_path: shell_zsh_path.clone(),
        main_execve_wrapper_exe: AbsolutePathBuf::from_absolute_path(if cfg!(windows) {
            r"C:\opt\codex\codex-execve-wrapper"
        } else {
            "/opt/codex/codex-execve-wrapper"
        })?,
    });

    let command = get_command(
        &args,
        Arc::new(default_user_shell()),
        &shell_mode,
        /*allow_login_shell*/ true,
    )
    .map_err(anyhow::Error::msg)?;

    assert_eq!(
        command,
        vec![
            shell_zsh_path.to_string_lossy().to_string(),
            "-lc".to_string(),
            "echo hello".to_string()
        ]
    );
    Ok(())
}

#[test]
fn exec_command_args_parse_execution_fields() -> anyhow::Result<()> {
    let json = r#"{
        "cmd": "echo hello",
        "tty": true,
        "yield_time_ms": 1234,
        "max_output_tokens": 250
    }"#;

    let args: ExecCommandArgs = parse_arguments(json)?;

    assert!(args.tty);
    assert_eq!(args.yield_time_ms, 1234);
    assert_eq!(args.max_output_tokens, Some(250));
    Ok(())
}

#[test]
fn write_stdin_args_parse_execution_fields() -> anyhow::Result<()> {
    let json = r#"{
        "session_id": 42,
        "chars": "echo hi\n",
        "yield_time_ms": 1234,
        "max_output_tokens": 250
    }"#;

    let args: WriteStdinArgs = parse_arguments(json)?;

    assert_eq!(args.session_id, 42);
    assert_eq!(args.chars, "echo hi\n");
    assert_eq!(args.yield_time_ms, 1234);
    assert_eq!(args.max_output_tokens, Some(250));
    Ok(())
}

#[test]
fn exec_command_args_reject_invalid_wait_until_terminal_type() {
    let json = r#"{
        "cmd": "echo hello",
        "wait_until_terminal": "true"
    }"#;

    let err = parse_arguments::<ExecCommandArgs>(json)
        .expect_err("wait_until_terminal must be parsed and typechecked");
    assert!(
        err.to_string().contains("wait_until_terminal"),
        "parse error should mention wait_until_terminal, got: {err}"
    );
}

#[test]
fn exec_command_args_reject_invalid_max_wait_ms_type() {
    let json = r#"{
        "cmd": "echo hello",
        "max_wait_ms": "1000"
    }"#;

    let err = parse_arguments::<ExecCommandArgs>(json)
        .expect_err("max_wait_ms must be parsed and typechecked");
    assert!(
        err.to_string().contains("max_wait_ms"),
        "parse error should mention max_wait_ms, got: {err}"
    );
}

#[test]
fn exec_command_args_reject_invalid_heartbeat_interval_ms_type() {
    let json = r#"{
        "cmd": "echo hello",
        "heartbeat_interval_ms": "100"
    }"#;

    let err = parse_arguments::<ExecCommandArgs>(json)
        .expect_err("heartbeat_interval_ms must be parsed and typechecked");
    assert!(
        err.to_string().contains("heartbeat_interval_ms"),
        "parse error should mention heartbeat_interval_ms, got: {err}"
    );
}

#[test]
fn write_stdin_args_reject_invalid_wait_until_terminal_type() {
    let json = r#"{
        "session_id": 42,
        "wait_until_terminal": "true"
    }"#;

    let err = parse_arguments::<WriteStdinArgs>(json)
        .expect_err("wait_until_terminal must be parsed and typechecked");
    assert!(
        err.to_string().contains("wait_until_terminal"),
        "parse error should mention wait_until_terminal, got: {err}"
    );
}

#[test]
fn write_stdin_args_reject_invalid_max_wait_ms_type() {
    let json = r#"{
        "session_id": 42,
        "max_wait_ms": "1000"
    }"#;

    let err = parse_arguments::<WriteStdinArgs>(json)
        .expect_err("max_wait_ms must be parsed and typechecked");
    assert!(
        err.to_string().contains("max_wait_ms"),
        "parse error should mention max_wait_ms, got: {err}"
    );
}

#[test]
fn write_stdin_args_reject_invalid_heartbeat_interval_ms_type() {
    let json = r#"{
        "session_id": 42,
        "heartbeat_interval_ms": "100"
    }"#;

    let err = parse_arguments::<WriteStdinArgs>(json)
        .expect_err("heartbeat_interval_ms must be parsed and typechecked");
    assert!(
        err.to_string().contains("heartbeat_interval_ms"),
        "parse error should mention heartbeat_interval_ms, got: {err}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_command_wait_until_terminal_blocks_until_process_exits() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));
    if cfg!(windows) {
        return Ok(());
    }

    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let started = Instant::now();
    let output = run_unified_exec(
        Arc::clone(&session),
        Arc::clone(&turn),
        "exec_command",
        serde_json::json!({
            "cmd": "sleep 1 && echo WAIT_UNTIL_TERMINAL_EXEC",
            "yield_time_ms": 250,
            "wait_until_terminal": true,
            "max_wait_ms": 5_000,
            "heartbeat_interval_ms": 100
        }),
    )
    .await
    .map_err(|err| anyhow::anyhow!("exec_command call failed: {err}"))?;

    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "wait_until_terminal should block close to command completion; got {:?}",
        started.elapsed()
    );
    assert!(
        output
            .truncated_output()
            .contains("WAIT_UNTIL_TERMINAL_EXEC"),
        "terminal wait should include command output"
    );
    assert!(
        output.process_id.is_none(),
        "terminal wait should not leave a resumable session for one-shot command"
    );
    assert_eq!(output.exit_code, Some(0));
    Ok(())
}

#[tokio::test]
async fn write_stdin_wait_until_terminal_requires_empty_chars() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let err = run_unified_exec(
        Arc::clone(&session),
        Arc::clone(&turn),
        "write_stdin",
        serde_json::json!({
            "session_id": 42,
            "chars": "echo should fail\n",
            "wait_until_terminal": true,
            "max_wait_ms": 5_000
        }),
    )
    .await
    .expect_err("wait_until_terminal with non-empty chars should be rejected");

    let FunctionCallError::RespondToModel(message) = err else {
        panic!("expected model-visible contract error, got: {err:?}");
    };
    assert!(
        message.contains("wait_until_terminal=true requires chars to be empty"),
        "unexpected error message: {message}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_command_wait_until_terminal_respects_max_wait_timeout() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));
    if cfg!(windows) {
        return Ok(());
    }

    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let started = Instant::now();
    let output = run_unified_exec(
        Arc::clone(&session),
        Arc::clone(&turn),
        "exec_command",
        serde_json::json!({
            "cmd": "sleep 5 && echo WAIT_UNTIL_TERMINAL_TIMEOUT_TOO_LATE",
            "yield_time_ms": 250,
            "wait_until_terminal": true,
            "max_wait_ms": 1_200,
            "heartbeat_interval_ms": 100
        }),
    )
    .await
    .map_err(|err| anyhow::anyhow!("exec_command call failed: {err}"))?;

    assert!(
        started.elapsed() >= Duration::from_millis(1_000),
        "wait_until_terminal should respect max_wait_ms before yielding; got {:?}",
        started.elapsed()
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "max_wait_ms timeout should return before command completion; got {:?}",
        started.elapsed()
    );
    assert!(
        !output
            .truncated_output()
            .contains("WAIT_UNTIL_TERMINAL_TIMEOUT_TOO_LATE"),
        "timed-out wait should not include output emitted after the wait window"
    );
    assert!(
        output.process_id.is_some(),
        "timed-out wait should keep a resumable session alive"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_stdin_wait_until_terminal_blocks_until_exit() -> anyhow::Result<()> {
    skip_if_sandbox!(Ok(()));
    if cfg!(windows) {
        return Ok(());
    }

    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn = Arc::new(turn);

    let open_shell = run_unified_exec(
        Arc::clone(&session),
        Arc::clone(&turn),
        "exec_command",
        serde_json::json!({
            "cmd": "bash -i",
            "yield_time_ms": 2_500
        }),
    )
    .await
    .map_err(|err| anyhow::anyhow!("exec_command opening shell failed: {err}"))?;
    let process_id = open_shell
        .process_id
        .expect("opening an interactive shell should return a session id");

    let started = Instant::now();
    let output = run_unified_exec(
        Arc::clone(&session),
        Arc::clone(&turn),
        "write_stdin",
        serde_json::json!({
            "session_id": process_id,
            "chars": "sleep 1 && echo WAIT_UNTIL_TERMINAL_WRITE && exit\n",
            "yield_time_ms": 250,
            "wait_until_terminal": true,
            "max_wait_ms": 5_000,
            "heartbeat_interval_ms": 100
        }),
    )
    .await
    .map_err(|err| anyhow::anyhow!("write_stdin call failed: {err}"))?;

    assert!(
        started.elapsed() >= Duration::from_millis(900),
        "write_stdin wait_until_terminal should wait for command completion; got {:?}",
        started.elapsed()
    );
    assert!(
        output
            .truncated_output()
            .contains("WAIT_UNTIL_TERMINAL_WRITE"),
        "waited write_stdin should include terminal output"
    );
    assert!(
        output.process_id.is_none(),
        "write_stdin wait_until_terminal should report terminal session as closed"
    );
    assert_eq!(output.exit_code, Some(0));
    Ok(())
}

#[test]
fn exec_command_args_resolve_relative_additional_permissions_against_workdir() -> anyhow::Result<()>
{
    let cwd = tempdir()?;
    let workdir = cwd.path().join("nested");
    fs::create_dir_all(&workdir)?;
    let expected_write = workdir.join("relative-write.txt");
    let json = r#"{
            "cmd": "echo hello",
            "workdir": "nested",
            "additional_permissions": {
                "file_system": {
                    "write": ["./relative-write.txt"]
                }
            }
        }"#;

    let base_path = resolve_workdir_base_path(json, &cwd.path().abs())?;
    let args: ExecCommandArgs = parse_arguments_with_base_path(json, &base_path)?;

    assert_eq!(
        args.additional_permissions,
        Some(PermissionProfile {
            file_system: Some(FileSystemPermissions::from_read_write_roots(
                /*read*/ None,
                Some(vec![expected_write.abs()]),
            )),
            ..Default::default()
        })
    );
    Ok(())
}

#[tokio::test]
async fn exec_command_pre_tool_use_payload_uses_raw_command() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "cmd": "printf exec command" }).to_string(),
    };
    let (session, turn) = make_session_and_context().await;
    let handler = UnifiedExecHandler;

    assert_eq!(
        handler.pre_tool_use_payload(&ToolInvocation {
            session: session.into(),
            turn: turn.into(),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-43".to_string(),
            tool_name: codex_tools::ToolName::plain("exec_command"),
            source: crate::tools::context::ToolCallSource::Direct,
            payload,
        }),
        Some(crate::tools::registry::PreToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_input: serde_json::json!({ "command": "printf exec command" }),
        })
    );
}

#[tokio::test]
async fn exec_command_pre_tool_use_payload_skips_write_stdin() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "chars": "echo hi" }).to_string(),
    };
    let (session, turn) = make_session_and_context().await;
    let handler = UnifiedExecHandler;

    assert_eq!(
        handler.pre_tool_use_payload(&ToolInvocation {
            session: session.into(),
            turn: turn.into(),
            cancellation_token: tokio_util::sync::CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-44".to_string(),
            tool_name: codex_tools::ToolName::plain("write_stdin"),
            source: crate::tools::context::ToolCallSource::Direct,
            payload,
        }),
        None
    );
}

#[tokio::test]
async fn exec_command_post_tool_use_payload_uses_output_for_noninteractive_one_shot_commands() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "cmd": "echo three", "tty": false }).to_string(),
    };
    let output = ExecCommandToolOutput {
        event_call_id: "call-43".to_string(),
        chunk_id: "chunk-1".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"three".to_vec(),
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: Some("echo three".to_string()),
    };
    let invocation = invocation_for_payload("exec_command", "call-43", payload).await;
    assert_eq!(
        UnifiedExecHandler.post_tool_use_payload(&invocation, &output),
        Some(crate::tools::registry::PostToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_use_id: "call-43".to_string(),
            tool_input: serde_json::json!({ "command": "echo three" }),
            tool_response: serde_json::json!("three"),
        })
    );
}

#[tokio::test]
async fn exec_command_post_tool_use_payload_uses_output_for_interactive_completion() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "cmd": "echo three", "tty": true }).to_string(),
    };
    let output = ExecCommandToolOutput {
        event_call_id: "call-44".to_string(),
        chunk_id: "chunk-1".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"three".to_vec(),
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: Some("echo three".to_string()),
    };
    let invocation = invocation_for_payload("exec_command", "call-44", payload).await;

    assert_eq!(
        UnifiedExecHandler.post_tool_use_payload(&invocation, &output),
        Some(crate::tools::registry::PostToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_use_id: "call-44".to_string(),
            tool_input: serde_json::json!({ "command": "echo three" }),
            tool_response: serde_json::json!("three"),
        })
    );
}

#[tokio::test]
async fn exec_command_post_tool_use_payload_skips_running_sessions() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "cmd": "echo three", "tty": false }).to_string(),
    };
    let output = ExecCommandToolOutput {
        event_call_id: "event-45".to_string(),
        chunk_id: "chunk-1".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"three".to_vec(),
        max_output_tokens: None,
        process_id: Some(45),
        exit_code: None,
        original_token_count: None,
        hook_command: Some("echo three".to_string()),
    };
    let invocation = invocation_for_payload("exec_command", "call-45", payload).await;
    assert_eq!(
        UnifiedExecHandler.post_tool_use_payload(&invocation, &output),
        None
    );
}

#[tokio::test]
async fn write_stdin_post_tool_use_payload_uses_original_exec_call_id_and_command_on_completion() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({
            "session_id": 45,
            "chars": "",
        })
        .to_string(),
    };
    let output = ExecCommandToolOutput {
        event_call_id: "exec-call-45".to_string(),
        chunk_id: "chunk-2".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"finished\n".to_vec(),
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: Some("sleep 1; echo finished".to_string()),
    };
    let invocation = invocation_for_payload("write_stdin", "write-stdin-call", payload).await;

    assert_eq!(
        UnifiedExecHandler.post_tool_use_payload(&invocation, &output),
        Some(crate::tools::registry::PostToolUsePayload {
            tool_name: HookToolName::bash(),
            tool_use_id: "exec-call-45".to_string(),
            tool_input: serde_json::json!({ "command": "sleep 1; echo finished" }),
            tool_response: serde_json::json!("finished\n"),
        })
    );
}

#[tokio::test]
async fn write_stdin_post_tool_use_payload_keeps_parallel_session_metadata_separate() {
    let payload = ToolPayload::Function {
        arguments: serde_json::json!({ "session_id": 45, "chars": "" }).to_string(),
    };
    let output_a = ExecCommandToolOutput {
        event_call_id: "exec-call-a".to_string(),
        chunk_id: "chunk-a".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"alpha\n".to_vec(),
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: Some("sleep 2; echo alpha".to_string()),
    };
    let output_b = ExecCommandToolOutput {
        event_call_id: "exec-call-b".to_string(),
        chunk_id: "chunk-b".to_string(),
        wall_time: std::time::Duration::from_millis(498),
        raw_output: b"beta\n".to_vec(),
        max_output_tokens: None,
        process_id: None,
        exit_code: Some(0),
        original_token_count: None,
        hook_command: Some("sleep 1; echo beta".to_string()),
    };
    let invocation_b = invocation_for_payload("write_stdin", "write-call-b", payload.clone()).await;
    let invocation_a = invocation_for_payload("write_stdin", "write-call-a", payload).await;

    let payloads = [
        UnifiedExecHandler.post_tool_use_payload(&invocation_b, &output_b),
        UnifiedExecHandler.post_tool_use_payload(&invocation_a, &output_a),
    ];

    assert_eq!(
        payloads,
        [
            Some(crate::tools::registry::PostToolUsePayload {
                tool_name: HookToolName::bash(),
                tool_use_id: "exec-call-b".to_string(),
                tool_input: serde_json::json!({ "command": "sleep 1; echo beta" }),
                tool_response: serde_json::json!("beta\n"),
            }),
            Some(crate::tools::registry::PostToolUsePayload {
                tool_name: HookToolName::bash(),
                tool_use_id: "exec-call-a".to_string(),
                tool_input: serde_json::json!({ "command": "sleep 2; echo alpha" }),
                tool_response: serde_json::json!("alpha\n"),
            }),
        ]
    );
}
