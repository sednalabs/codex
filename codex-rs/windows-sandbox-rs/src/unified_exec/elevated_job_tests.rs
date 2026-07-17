#![cfg(target_os = "windows")]

use super::current_thread_runtime;
use super::job_test_support::GrandchildFixture;
use super::job_test_support::SessionEnding;
use super::job_test_support::SessionMode;
use super::job_test_support::assert_grandchild_stopped;
use super::job_test_support::grandchild_fixture;
use super::job_test_support::windows_powershell_path;
use super::job_test_support::windows_process_test_guard;
use super::sandbox_cwd;
use super::sandbox_log;
use super::workspace_roots_for;
use crate::WindowsSandboxProxySettingsMode;
use crate::ipc_framed::Message;
use crate::ipc_framed::OutputStream;
use crate::ipc_framed::SpawnRequest;
use crate::ipc_framed::decode_bytes;
use crate::ipc_framed::read_frame;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::runner_client::RunnerTransport;
use crate::runner_client::spawn_runner_transport;
use crate::spawn_prep::prepare_elevated_spawn_context_for_permissions;
use crate::unified_exec::spawn_windows_sandbox_session_elevated_for_permission_profile;
use anyhow::Context;
use codex_protocol::models::PermissionProfile;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio::time::timeout;

static ELEVATED_TEST_CODEX_HOME: OnceLock<PathBuf> = OnceLock::new();
const DIAGNOSTIC_OUTPUT_CAP: usize = 8 * 1024;

fn elevated_test_codex_home() -> &'static Path {
    ELEVATED_TEST_CODEX_HOME
        .get_or_init(|| {
            let path = if let Some(test_tmpdir) = std::env::var_os("TEST_TMPDIR") {
                // Elevated setup provisions machine-local users. Bazel retries reuse the same
                // Windows VM, so keep CODEX_HOME stable and reconcile its persisted ACL state.
                PathBuf::from(test_tmpdir).join("elevated-job-lifecycle-codex-home")
            } else {
                std::env::temp_dir().join(format!(
                    "codex-windows-sandbox-elevated-job-lifecycle-{}",
                    std::process::id()
                ))
            };
            fs::create_dir_all(&path).unwrap_or_else(|err| {
                panic!(
                    "create stable elevated test CODEX_HOME {}: {err}",
                    path.display()
                )
            });
            path
        })
        .as_path()
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
        let destination = resources_dir.join(file_name);
        if let Err(err) = fs::copy(&helper, &destination) {
            // A runner from a preceding Bazel retry can briefly retain the staged executable.
            // In that case the existing copy is the binary that retry already launched.
            if err.kind() == std::io::ErrorKind::PermissionDenied && destination.exists() {
                continue;
            }
            return Err(err).with_context(|| {
                format!(
                    "stage Windows sandbox helper {} at {}",
                    helper.display(),
                    destination.display()
                )
            });
        }
    }
    Ok(())
}

fn append_diagnostic_output(buffer: &mut Vec<u8>, chunk: &[u8]) {
    let remaining = DIAGNOSTIC_OUTPUT_CAP.saturating_sub(buffer.len());
    buffer.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
}

#[derive(Debug)]
enum RunnerTerminal {
    Exit { exit_code: i32, timed_out: bool },
    Error(String),
}

enum RunnerProbeEvent {
    Output { stream: OutputStream, data: Vec<u8> },
    Terminal(RunnerTerminal),
}

struct RunnerProbe {
    events: std::sync::mpsc::Receiver<RunnerProbeEvent>,
    reader: Option<std::thread::JoinHandle<()>>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    terminal: Option<RunnerTerminal>,
}

impl RunnerProbe {
    fn start(mut pipe_read: std::fs::File) -> Self {
        let (event_tx, events) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let terminal = loop {
                match read_frame(&mut pipe_read) {
                    Ok(Some(frame)) => match frame.message {
                        Message::Output { payload } => match decode_bytes(&payload.data_b64) {
                            Ok(data) => {
                                if event_tx
                                    .send(RunnerProbeEvent::Output {
                                        stream: payload.stream,
                                        data,
                                    })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            Err(err) => {
                                break RunnerTerminal::Error(format!(
                                    "decode runner {:?} output: {err}",
                                    payload.stream
                                ));
                            }
                        },
                        Message::Exit { payload } => {
                            break RunnerTerminal::Exit {
                                exit_code: payload.exit_code,
                                timed_out: payload.timed_out,
                            };
                        }
                        Message::Error { payload } => {
                            break RunnerTerminal::Error(format!(
                                "runner {:?} error {:?}: {}",
                                payload.stage, payload.windows_error_code, payload.message
                            ));
                        }
                        Message::SpawnReady { .. }
                        | Message::SpawnRequest { .. }
                        | Message::Stdin { .. }
                        | Message::CloseStdin { .. }
                        | Message::Resize { .. }
                        | Message::Terminate { .. } => {}
                    },
                    Ok(None) => {
                        break RunnerTerminal::Error(
                            "runner pipe closed before terminal result".to_string(),
                        );
                    }
                    Err(err) => {
                        break RunnerTerminal::Error(format!("read runner frame: {err:#}"));
                    }
                }
            };
            let _ = event_tx.send(RunnerProbeEvent::Terminal(terminal));
        });
        Self {
            events,
            reader: Some(reader),
            stdout: Vec::new(),
            stderr: Vec::new(),
            terminal: None,
        }
    }

    fn handle_event(&mut self, event: RunnerProbeEvent) {
        match event {
            RunnerProbeEvent::Output { stream, data } => match stream {
                OutputStream::Stdout => append_diagnostic_output(&mut self.stdout, &data),
                OutputStream::Stderr => append_diagnostic_output(&mut self.stderr, &data),
            },
            RunnerProbeEvent::Terminal(terminal) => self.terminal = Some(terminal),
        }
    }

    fn drain(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.handle_event(event);
        }
    }

    fn diagnostics(&mut self) -> String {
        self.drain();
        format!(
            "terminal={:?}, stdout={:?}, stderr={:?}",
            self.terminal,
            String::from_utf8_lossy(&self.stdout),
            String::from_utf8_lossy(&self.stderr)
        )
    }

    fn wait_for_terminal_result(&mut self) -> (i32, bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.terminal.is_none() && Instant::now() < deadline {
            match self.events.recv_timeout(Duration::from_millis(25)) {
                Ok(event) => self.handle_event(event),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        self.drain();
        let terminal = self.terminal.take().unwrap_or_else(|| {
            panic!(
                "timed out waiting for runner terminal result: {}",
                self.diagnostics()
            )
        });
        if let Some(reader) = self.reader.take() {
            reader.join().expect("runner output reader should finish");
        }
        match terminal {
            RunnerTerminal::Exit {
                exit_code,
                timed_out,
            } => (exit_code, timed_out),
            RunnerTerminal::Error(message) => panic!("{message}: {}", self.diagnostics()),
        }
    }
}

fn spawn_pipe_backed_grandchild(
    cwd: &Path,
    powershell: &Path,
    codex_home: &Path,
    timeout_ms: u64,
) -> (GrandchildFixture, RunnerTransport) {
    let fixture = grandchild_fixture(cwd, powershell, "Start-Sleep -Seconds 30");
    let permission_profile = PermissionProfile::workspace_write();
    let workspace_roots = workspace_roots_for(cwd);
    let mut env_map = HashMap::new();
    let permissions =
        ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
            &permission_profile,
            &workspace_roots,
        )
        .expect("resolve elevated test permissions");
    let elevated = prepare_elevated_spawn_context_for_permissions(
        permissions,
        codex_home,
        cwd,
        &mut env_map,
        &fixture.command,
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
            "prepare elevated pipe-backed session: {err:#}\n{}",
            sandbox_log(codex_home)
        )
    });
    let spawn_request = SpawnRequest {
        command: fixture.command.clone(),
        cwd: cwd.to_path_buf(),
        env: env_map,
        permission_profile,
        workspace_roots,
        codex_home: elevated.sandbox_base.clone(),
        real_codex_home: codex_home.to_path_buf(),
        cap_sids: elevated.cap_sids.clone(),
        timeout_ms: Some(timeout_ms),
        tty: false,
        stdin_open: false,
        use_private_desktop: false,
    };
    let transport = spawn_runner_transport(
        codex_home,
        cwd,
        &elevated.sandbox_creds,
        elevated.logs_base_dir.as_deref(),
        spawn_request,
    )
    .unwrap_or_else(|err| {
        panic!(
            "spawn elevated runner transport: {err:#}\n{}",
            sandbox_log(codex_home)
        )
    });
    (fixture, transport)
}

async fn wait_for_spawned_grandchild(
    fixture: &GrandchildFixture,
    session: &codex_utils_pty::ProcessHandle,
    stdout_rx: &mut mpsc::Receiver<Vec<u8>>,
    stderr_rx: &mut mpsc::Receiver<Vec<u8>>,
    codex_home: &Path,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        while let Ok(chunk) = stdout_rx.try_recv() {
            append_diagnostic_output(&mut stdout, &chunk);
        }
        while let Ok(chunk) = stderr_rx.try_recv() {
            append_diagnostic_output(&mut stderr, &chunk);
        }
        let readiness = fixture.readiness();
        if readiness.is_complete() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "grandchild startup incomplete: readiness={readiness:?}, early_exit={:?}, timed_out=not_exposed, stdout={:?}, stderr={:?}\n{}",
                session.exit_code(),
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr),
                sandbox_log(codex_home)
            );
        }
        sleep(Duration::from_millis(25)).await;
    }
}

fn wait_for_runner_grandchild(
    fixture: &GrandchildFixture,
    probe: &mut RunnerProbe,
    codex_home: &Path,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        probe.drain();
        let readiness = fixture.readiness();
        if readiness.is_complete() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "grandchild startup incomplete: readiness={readiness:?}, {}\n{}",
                probe.diagnostics(),
                sandbox_log(codex_home)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn assert_elevated_session_stops_grandchild(mode: SessionMode, ending: SessionEnding) {
    let _guard = windows_process_test_guard();
    stage_windows_sandbox_helpers().expect("stage elevated sandbox helpers");
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let powershell = windows_powershell_path();
        let fixture = grandchild_fixture(&cwd, &powershell, ending.root_tail());
        let codex_home = elevated_test_codex_home();
        let permission_profile = PermissionProfile::workspace_write();
        let spawned = spawn_windows_sandbox_session_elevated_for_permission_profile(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home,
            fixture.command.clone(),
            cwd.as_path(),
            HashMap::new(),
            /*proxy_enforced*/ false,
            Some(30_000),
            /*read_roots_override*/ None,
            /*read_roots_include_platform_defaults*/ false,
            /*write_roots_override*/ None,
            &[],
            &[],
            /*tty*/ mode.tty(),
            /*stdin_open*/ mode.tty(),
            /*use_private_desktop*/ false,
        )
        .await
        .unwrap_or_else(|err| {
            panic!(
                "spawn elevated {} grandchild session: {err:#}\n{}",
                mode.label(),
                sandbox_log(codex_home)
            )
        });

        let codex_utils_pty::SpawnedProcess {
            session,
            mut stdout_rx,
            mut stderr_rx,
            exit_rx,
        } = spawned;
        wait_for_spawned_grandchild(
            &fixture,
            &session,
            &mut stdout_rx,
            &mut stderr_rx,
            codex_home,
        )
        .await;
        if matches!(ending, SessionEnding::ExplicitTermination) {
            session.request_terminate();
        }
        let exit_code = timeout(Duration::from_secs(10), exit_rx)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "timed out waiting for elevated session exit\n{}",
                    sandbox_log(codex_home)
                )
            })
            .unwrap_or(-1);
        match ending {
            SessionEnding::ExplicitTermination => assert_ne!(exit_code, 0),
            SessionEnding::RootExit => assert_eq!(exit_code, 0),
        }
        assert_grandchild_stopped(&fixture);
    });
}

#[test]
fn elevated_non_tty_termination_stops_grandchild() {
    assert_elevated_session_stops_grandchild(SessionMode::Pipe, SessionEnding::ExplicitTermination);
}

#[test]
fn elevated_tty_termination_stops_grandchild() {
    assert_elevated_session_stops_grandchild(SessionMode::Tty, SessionEnding::ExplicitTermination);
}

#[test]
fn elevated_non_tty_root_exit_stops_grandchild() {
    assert_elevated_session_stops_grandchild(SessionMode::Pipe, SessionEnding::RootExit);
}

#[test]
fn elevated_tty_root_exit_stops_grandchild() {
    assert_elevated_session_stops_grandchild(SessionMode::Tty, SessionEnding::RootExit);
}

#[test]
fn elevated_control_transport_eof_stops_grandchild() {
    let _guard = windows_process_test_guard();
    stage_windows_sandbox_helpers().expect("stage elevated sandbox helpers");
    let cwd = sandbox_cwd();
    let powershell = windows_powershell_path();
    let codex_home = elevated_test_codex_home();
    let (fixture, transport) =
        spawn_pipe_backed_grandchild(&cwd, &powershell, codex_home, /*timeout_ms*/ 30_000);

    let (pipe_write, pipe_read) = transport.into_files();
    let mut probe = RunnerProbe::start(pipe_read);
    wait_for_runner_grandchild(&fixture, &mut probe, codex_home);
    drop(pipe_write);

    let (exit_code, timed_out) = probe.wait_for_terminal_result();
    assert_ne!(exit_code, 0);
    assert!(!timed_out);
    assert_grandchild_stopped(&fixture);
}

#[test]
fn elevated_pipe_timeout_stops_grandchild_and_reports_terminal_result() {
    let _guard = windows_process_test_guard();
    stage_windows_sandbox_helpers().expect("stage elevated sandbox helpers");
    let cwd = sandbox_cwd();
    let powershell = windows_powershell_path();
    let codex_home = elevated_test_codex_home();
    let (fixture, transport) =
        spawn_pipe_backed_grandchild(&cwd, &powershell, codex_home, /*timeout_ms*/ 5_000);

    let (_pipe_write, pipe_read) = transport.into_files();
    let mut probe = RunnerProbe::start(pipe_read);
    wait_for_runner_grandchild(&fixture, &mut probe, codex_home);

    assert_eq!(probe.wait_for_terminal_result(), (192, true));
    assert_grandchild_stopped(&fixture);
}
