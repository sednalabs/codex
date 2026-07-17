#![cfg(target_os = "windows")]

use super::legacy_process_test_guard;
use super::sandbox_cwd;
use super::sandbox_home;
use super::sandbox_log;
use super::workspace_roots_for;
use crate::WindowsSandboxProxySettingsMode;
use crate::ipc_framed::ErrorStage;
use crate::ipc_framed::Message;
use crate::ipc_framed::OutputStream;
use crate::ipc_framed::SpawnRequest;
use crate::ipc_framed::decode_bytes;
use crate::ipc_framed::read_frame;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::runner_client::RunnerTransport;
use crate::runner_client::spawn_runner_transport;
use crate::spawn_prep::prepare_elevated_spawn_context_for_permissions;
use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_protocol::models::PermissionProfile;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::FALSE;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::System::Threading::GetExitCodeProcess;
use windows_sys::Win32::System::Threading::OpenProcess;
use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

const CHILD_TIMEOUT: Duration = Duration::from_secs(5);
const OBSERVATION_TIMEOUT: Duration = Duration::from_secs(10);
const OUTPUT_CAP: usize = 8 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExitCodeObservation {
    ExitCode(u32),
    OpenFailed(u32),
    QueryFailed(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TerminalObservation {
    Exit {
        exit_code: i32,
        timed_out: bool,
    },
    RunnerError {
        stage: ErrorStage,
        windows_error_code: Option<u32>,
        message: String,
    },
    PipeClosed,
    ReadError(String),
    WaitTimedOut {
        child_exit_code: ExitCodeObservation,
    },
}

enum ProbeEvent {
    Output {
        stream: OutputStream,
        data: Vec<u8>,
    },
    Terminal(TerminalObservation),
}

#[derive(Debug)]
struct ChildObservation {
    label: &'static str,
    spawn_ready_process_id: u32,
    stdout: String,
    stderr: String,
    terminal: TerminalObservation,
}

#[derive(Debug, PartialEq, Eq)]
struct ChildSummary {
    label: &'static str,
    spawn_ready_process_id_nonzero: bool,
    stdout: String,
    stderr: String,
    terminal: TerminalObservation,
}

impl ChildObservation {
    fn summary(&self) -> ChildSummary {
        ChildSummary {
            label: self.label,
            spawn_ready_process_id_nonzero: self.spawn_ready_process_id != 0,
            stdout: self.stdout.trim_end().to_string(),
            stderr: self.stderr.clone(),
            terminal: self.terminal.clone(),
        }
    }
}

struct RunnerProbe {
    events: mpsc::Receiver<ProbeEvent>,
    reader: Option<thread::JoinHandle<()>>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl RunnerProbe {
    fn start(mut pipe_read: fs::File) -> Self {
        let (event_tx, events) = mpsc::channel();
        let reader = thread::spawn(move || {
            let terminal = loop {
                match read_frame(&mut pipe_read) {
                    Ok(Some(frame)) => match frame.message {
                        Message::Output { payload } => match decode_bytes(&payload.data_b64) {
                            Ok(data) => {
                                if event_tx
                                    .send(ProbeEvent::Output {
                                        stream: payload.stream,
                                        data,
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(err) => {
                                break TerminalObservation::ReadError(format!(
                                    "decode {:?} output: {err}",
                                    payload.stream
                                ));
                            }
                        },
                        Message::Exit { payload } => {
                            break TerminalObservation::Exit {
                                exit_code: payload.exit_code,
                                timed_out: payload.timed_out,
                            };
                        }
                        Message::Error { payload } => {
                            break TerminalObservation::RunnerError {
                                stage: payload.stage,
                                windows_error_code: payload.windows_error_code,
                                message: payload.message,
                            };
                        }
                        Message::SpawnReady { .. }
                        | Message::SpawnRequest { .. }
                        | Message::Stdin { .. }
                        | Message::CloseStdin { .. }
                        | Message::Resize { .. }
                        | Message::Terminate { .. } => {}
                    },
                    Ok(None) => break TerminalObservation::PipeClosed,
                    Err(err) => {
                        break TerminalObservation::ReadError(format!(
                            "read runner frame: {err:#}"
                        ));
                    }
                }
            };
            let _ = event_tx.send(ProbeEvent::Terminal(terminal));
        });
        Self {
            events,
            reader: Some(reader),
            stdout: Vec::new(),
            stderr: Vec::new(),
        }
    }

    fn observe(mut self, process_id: u32) -> (String, String, TerminalObservation) {
        let deadline = Instant::now() + OBSERVATION_TIMEOUT;
        let terminal = loop {
            let now = Instant::now();
            if now >= deadline {
                break TerminalObservation::WaitTimedOut {
                    child_exit_code: query_process_exit_code(process_id),
                };
            }
            match self.events.recv_timeout(deadline - now) {
                Ok(ProbeEvent::Output { stream, data }) => match stream {
                    OutputStream::Stdout => append_output(&mut self.stdout, &data),
                    OutputStream::Stderr => append_output(&mut self.stderr, &data),
                },
                Ok(ProbeEvent::Terminal(terminal)) => {
                    if let Some(reader) = self.reader.take() {
                        reader.join().expect("runner frame reader should finish");
                    }
                    break terminal;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    break TerminalObservation::WaitTimedOut {
                        child_exit_code: query_process_exit_code(process_id),
                    };
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break TerminalObservation::PipeClosed;
                }
            }
        };
        (
            String::from_utf8_lossy(&self.stdout).into_owned(),
            String::from_utf8_lossy(&self.stderr).into_owned(),
            terminal,
        )
    }
}

fn append_output(buffer: &mut Vec<u8>, chunk: &[u8]) {
    let remaining = OUTPUT_CAP.saturating_sub(buffer.len());
    buffer.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
}

fn query_process_exit_code(process_id: u32) -> ExitCodeObservation {
    // Keep timeout telemetry observational: this handle cannot terminate or modify the child.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, process_id) };
    if process == 0 {
        return ExitCodeObservation::OpenFailed(unsafe { GetLastError() });
    }

    let mut exit_code = 0;
    let query_result = unsafe { GetExitCodeProcess(process, &mut exit_code) };
    let query_error = unsafe { GetLastError() };
    unsafe {
        CloseHandle(process);
    }
    if query_result == 0 {
        ExitCodeObservation::QueryFailed(query_error)
    } else {
        ExitCodeObservation::ExitCode(exit_code)
    }
}

fn stage_windows_sandbox_helpers() -> anyhow::Result<()> {
    let test_exe = std::env::current_exe().context("resolve current Windows test executable")?;
    let test_exe_dir = test_exe
        .parent()
        .context("Windows test executable should have a parent directory")?;
    let resources_dir = test_exe_dir.join("codex-resources");
    match fs::create_dir_all(&resources_dir) {
        Ok(()) => {}
        Err(err)
            if err.kind() == std::io::ErrorKind::PermissionDenied && resources_dir.is_dir() => {}
        Err(err) => {
            return Err(err)
                .with_context(|| format!("create resources dir {}", resources_dir.display()));
        }
    }

    for helper_name in ["codex-windows-sandbox-setup", "codex-command-runner"] {
        let file_name = Path::new(helper_name).with_extension("exe");
        let helper = match codex_utils_cargo_bin::cargo_bin(helper_name) {
            Ok(helper) => helper,
            Err(cargo_bin_err) if codex_utils_cargo_bin::runfiles_available() => {
                codex_utils_cargo_bin::resolve_bazel_runfile(
                    option_env!("BAZEL_PACKAGE"),
                    &file_name,
                )
                .with_context(|| {
                    format!(
                        "resolve Bazel runfile for {helper_name} after cargo_bin failed: {cargo_bin_err}"
                    )
                })?
            }
            Err(err) => return Err(err.into()),
        };
        let destination = resources_dir.join(&file_name);
        if let Err(err) = fs::copy(&helper, &destination) {
            // A retry can briefly retain the already-staged helper executable.
            if err.kind() == std::io::ErrorKind::PermissionDenied && destination.exists() {
                continue;
            }
            return Err(err).with_context(|| {
                format!(
                    "stage Windows sandbox helper {} in {}",
                    helper.display(),
                    resources_dir.display()
                )
            });
        }
    }
    Ok(())
}

fn encoded_powershell_command(script: &str) -> String {
    let bytes = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    STANDARD.encode(bytes)
}

fn observe_child(
    label: &'static str,
    codex_home: &Path,
    cwd: &Path,
    command: Vec<String>,
) -> ChildObservation {
    let permission_profile = PermissionProfile::workspace_write();
    let workspace_roots = workspace_roots_for(cwd);
    let mut env_map = HashMap::new();
    let permissions =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            &permission_profile,
            &workspace_roots,
        )
        .expect("resolve elevated child probe permissions");
    let elevated = prepare_elevated_spawn_context_for_permissions(
        permissions,
        codex_home,
        cwd,
        &mut env_map,
        &command,
        /*read_roots_override*/ None,
        /*read_roots_include_platform_defaults*/ false,
        /*write_roots_override*/ None,
        &[],
        &[],
        /*proxy_enforced*/ false,
        WindowsSandboxProxySettingsMode::Reconcile,
    )
    .unwrap_or_else(|err| {
        panic!(
            "prepare {label} elevated child probe: {err:#}\n{}",
            sandbox_log(codex_home)
        )
    });
    let request = SpawnRequest {
        command,
        cwd: cwd.to_path_buf(),
        env: env_map,
        permission_profile,
        workspace_roots,
        codex_home: elevated.sandbox_base.clone(),
        real_codex_home: codex_home.to_path_buf(),
        cap_sids: elevated.cap_sids.clone(),
        timeout_ms: Some(CHILD_TIMEOUT.as_millis() as u64),
        tty: false,
        stdin_open: false,
        use_private_desktop: false,
    };
    let transport = spawn_runner_transport(
        codex_home,
        cwd,
        &elevated.sandbox_creds,
        elevated.logs_base_dir.as_deref(),
        request,
    )
    .unwrap_or_else(|err| {
        panic!(
            "spawn {label} elevated child probe: {err:#}\n{}",
            sandbox_log(codex_home)
        )
    });
    observe_transport(label, transport)
}

fn observe_transport(label: &'static str, transport: RunnerTransport) -> ChildObservation {
    let spawn_ready_process_id = transport
        .spawn_ready_process_id()
        .expect("runner transport should retain the spawn-ready process id in tests");
    let (pipe_write, pipe_read) = transport.into_files();
    let (stdout, stderr, terminal) = RunnerProbe::start(pipe_read).observe(spawn_ready_process_id);
    drop(pipe_write);
    ChildObservation {
        label,
        spawn_ready_process_id,
        stdout,
        stderr,
        terminal,
    }
}

#[test]
fn elevated_child_transport_reports_cmd_and_windows_powershell_startup() {
    let guard = legacy_process_test_guard();
    stage_windows_sandbox_helpers().expect("stage elevated child probe helpers");
    let cwd = sandbox_cwd();
    let codex_home = sandbox_home("elevated-child-telemetry");
    let system_root = std::env::var_os("SystemRoot").expect("SystemRoot should be set on Windows");
    let system_root = PathBuf::from(system_root);

    let cmd = observe_child(
        "cmd",
        codex_home.path(),
        &cwd,
        vec![
            system_root.join("System32\\cmd.exe").display().to_string(),
            "/d".to_string(),
            "/q".to_string(),
            "/c".to_string(),
            "echo ELEVATED-CMD-READY".to_string(),
        ],
    );
    let powershell = observe_child(
        "windows-powershell",
        codex_home.path(),
        &cwd,
        vec![
            system_root
                .join("System32\\WindowsPowerShell\\v1.0\\powershell.exe")
                .display()
                .to_string(),
            "-NoProfile".to_string(),
            "-EncodedCommand".to_string(),
            encoded_powershell_command("Write-Output 'ELEVATED-POWERSHELL-READY'; exit 0"),
        ],
    );
    drop(guard);

    println!("cmd observation: {cmd:#?}");
    println!("windows powershell observation: {powershell:#?}");
    assert_eq!(
        [cmd.summary(), powershell.summary()],
        [
            ChildSummary {
                label: "cmd",
                spawn_ready_process_id_nonzero: true,
                stdout: "ELEVATED-CMD-READY".to_string(),
                stderr: String::new(),
                terminal: TerminalObservation::Exit {
                    exit_code: 0,
                    timed_out: false,
                },
            },
            ChildSummary {
                label: "windows-powershell",
                spawn_ready_process_id_nonzero: true,
                stdout: "ELEVATED-POWERSHELL-READY".to_string(),
                stderr: String::new(),
                terminal: TerminalObservation::Exit {
                    exit_code: 0,
                    timed_out: false,
                },
            },
        ],
        "{}",
        sandbox_log(codex_home.path())
    );
}
