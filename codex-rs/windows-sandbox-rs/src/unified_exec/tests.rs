#![cfg(target_os = "windows")]

use super::spawn_windows_sandbox_session_legacy;
use crate::WindowsSandboxCancellationToken;
use crate::acl::path_mask_allows;
use crate::ipc_framed::Message;
use crate::ipc_framed::decode_bytes;
use crate::ipc_framed::read_frame;
use crate::resolved_permissions::ResolvedWindowsSandboxPermissions;
use crate::run_windows_sandbox_capture;
use crate::spawn_prep::legacy_session_capability_roots;
use crate::spawn_prep::root_capability_sids;
use crate::token::get_current_token_for_restriction;
use crate::winutil::string_from_sid_bytes;
use crate::winutil::to_wide;
use anyhow::Result;
use codex_protocol::models::PermissionProfile;
use codex_utils_absolute_path::AbsolutePathBuf;
use codex_utils_pty::ProcessDriver;
use pretty_assertions::assert_eq;
use std::collections::HashMap;
use std::ffi::c_void;
use std::fs;
use std::fs::OpenOptions;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;
use tokio::runtime::Builder;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;
use windows_sys::Win32::Foundation::CloseHandle;
use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::Foundation::GetLastError;
use windows_sys::Win32::Foundation::HLOCAL;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::ACL;
use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
use windows_sys::Win32::Security::Authorization::SDDL_REVISION_1;
use windows_sys::Win32::Security::Authorization::SE_FILE_OBJECT;
use windows_sys::Win32::Security::Authorization::SetNamedSecurityInfoW;
use windows_sys::Win32::Security::CopySid;
use windows_sys::Win32::Security::DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::GetLengthSid;
use windows_sys::Win32::Security::GetSecurityDescriptorDacl;
use windows_sys::Win32::Security::GetTokenInformation;
use windows_sys::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION;
use windows_sys::Win32::Security::TOKEN_USER;
use windows_sys::Win32::Security::TokenUser;
use windows_sys::Win32::Storage::FileSystem::DELETE;
use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
use windows_sys::Win32::Storage::FileSystem::FILE_DELETE_CHILD;

static TEST_HOME_COUNTER: AtomicU64 = AtomicU64::new(0);
static LEGACY_PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

fn legacy_process_test_guard() -> MutexGuard<'static, ()> {
    LEGACY_PROCESS_TEST_LOCK
        .lock()
        .expect("legacy Windows sandbox process test lock poisoned")
}

fn current_thread_runtime() -> tokio::runtime::Runtime {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime")
}

fn pwsh_path() -> Option<PathBuf> {
    let program_files = std::env::var_os("ProgramFiles")?;
    let path = PathBuf::from(program_files).join("PowerShell\\7\\pwsh.exe");
    path.is_file().then_some(path)
}

fn sandbox_cwd() -> PathBuf {
    if let Ok(workspace_root) = std::env::var("INSTA_WORKSPACE_ROOT") {
        return PathBuf::from(workspace_root);
    }

    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn sandbox_home(name: &str) -> TempDir {
    let id = TEST_HOME_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("codex-windows-sandbox-{name}-{id}"));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("create sandbox home");
    tempfile::TempDir::new_in(&path).expect("create sandbox home tempdir")
}

fn current_user_sid() -> Result<Vec<u8>> {
    unsafe {
        let token = get_current_token_for_restriction()?;
        let result = (|| {
            let mut needed = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
            anyhow::ensure!(needed >= std::mem::size_of::<TOKEN_USER>() as u32);

            let mut token_user_bytes = vec![0; needed as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                token_user_bytes.as_mut_ptr() as *mut c_void,
                needed,
                &mut needed,
            );
            anyhow::ensure!(
                ok != 0,
                "GetTokenInformation(TokenUser) failed: {}",
                GetLastError()
            );
            let token_user =
                std::ptr::read_unaligned(token_user_bytes.as_ptr() as *const TOKEN_USER);
            let sid_len = GetLengthSid(token_user.User.Sid);
            anyhow::ensure!(sid_len != 0, "GetLengthSid failed: {}", GetLastError());

            let mut sid = vec![0; sid_len as usize];
            let copied = CopySid(
                sid_len,
                sid.as_mut_ptr() as *mut c_void,
                token_user.User.Sid,
            );
            anyhow::ensure!(copied != 0, "CopySid failed: {}", GetLastError());
            Ok(sid)
        })();
        let _ = CloseHandle(token);
        result
    }
}

fn replace_with_restrictive_test_dacl(path: &Path, current_user_sid: &[u8]) -> Result<()> {
    let current_user_sid = string_from_sid_bytes(current_user_sid).map_err(anyhow::Error::msg)?;
    let sddl = to_wide(format!(
        "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;{current_user_sid})"
    ));
    unsafe {
        let mut security_descriptor = std::ptr::null_mut();
        let converted = ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut security_descriptor,
            std::ptr::null_mut(),
        );
        anyhow::ensure!(
            converted != 0,
            "ConvertStringSecurityDescriptorToSecurityDescriptorW failed: {}",
            GetLastError()
        );

        let result = (|| {
            let mut dacl_present = 0;
            let mut dacl_defaulted = 0;
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let found = GetSecurityDescriptorDacl(
                security_descriptor,
                &mut dacl_present,
                &mut dacl,
                &mut dacl_defaulted,
            );
            anyhow::ensure!(found != 0 && dacl_present != 0 && !dacl.is_null());

            let mut path_wide = to_wide(path);
            let status = SetNamedSecurityInfoW(
                path_wide.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                dacl,
                std::ptr::null_mut(),
            );
            anyhow::ensure!(
                status == ERROR_SUCCESS,
                "SetNamedSecurityInfoW failed: {status}"
            );
            Ok(())
        })();
        let _ = LocalFree(security_descriptor as HLOCAL);
        result
    }
}

fn declared_windows_test_root(name: &str) -> (TempDir, Vec<u8>) {
    let base = std::env::var_os("TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // Outside Bazel, use Windows' process temp directory. The unique
            // child is ACL-normalized before any test fixtures are created.
            std::env::temp_dir()
        });
    fs::create_dir_all(&base).expect("create declared Windows test temp base");
    let test_root = tempfile::Builder::new()
        .prefix(&format!("codex-{name}-"))
        .tempdir_in(base)
        .expect("create declared Windows test root");
    let current_user_sid = current_user_sid().expect("resolve current test user SID");
    replace_with_restrictive_test_dacl(test_root.path(), &current_user_sid)
        .expect("normalize test root DACL");
    (test_root, current_user_sid)
}

fn sandbox_log(codex_home: &Path) -> String {
    let log_path = crate::current_log_file_path(&codex_home.join(".sandbox"));
    fs::read_to_string(&log_path)
        .unwrap_or_else(|err| format!("failed to read {}: {err}", log_path.display()))
}

fn workspace_roots_for(root: &Path) -> Vec<AbsolutePathBuf> {
    vec![AbsolutePathBuf::from_absolute_path(root).expect("absolute workspace root")]
}

fn wait_for_frame_count(frames_path: &Path, expected_frames: usize) -> Vec<Message> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let mut reader = OpenOptions::new()
            .read(true)
            .open(frames_path)
            .expect("open frame file for read");
        reader
            .seek(SeekFrom::Start(0))
            .expect("seek to start of frame file");

        let mut frames = Vec::new();
        loop {
            match read_frame(&mut reader) {
                Ok(Some(frame)) => frames.push(frame.message),
                Ok(None) => break,
                Err(_) => break,
            }
        }

        if frames.len() >= expected_frames {
            return frames;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {expected_frames} frames, saw {}",
            frames.len()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

async fn collect_stdout_and_exit(
    spawned: codex_utils_pty::SpawnedProcess,
    codex_home: &Path,
    timeout_duration: Duration,
) -> (Vec<u8>, i32) {
    let codex_utils_pty::SpawnedProcess {
        session: _session,
        mut stdout_rx,
        stderr_rx: _stderr_rx,
        exit_rx,
    } = spawned;
    let stdout_task = tokio::spawn(async move {
        let mut stdout = Vec::new();
        while let Some(chunk) = stdout_rx.recv().await {
            stdout.extend(chunk);
        }
        stdout
    });
    let exit_code = timeout(timeout_duration, exit_rx)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for exit\n{}", sandbox_log(codex_home)))
        .unwrap_or(-1);
    let stdout = timeout(timeout_duration, stdout_task)
        .await
        .unwrap_or_else(|_| {
            panic!(
                "timed out waiting for stdout task\n{}",
                sandbox_log(codex_home)
            )
        })
        .expect("stdout task join");
    (stdout, exit_code)
}

#[test]
fn legacy_non_tty_cmd_emits_output() {
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-non-tty-cmd");
        println!("cmd codex_home={}", codex_home.path().display());
        let permission_profile = PermissionProfile::workspace_write();
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                "/c".to_string(),
                "echo LEGACY-NONTTY-CMD".to_string(),
            ],
            cwd.as_path(),
            HashMap::new(),
            /*timeout_ms*/ Some(5_000),
            /*additional_deny_read_paths*/ &[],
            /*additional_deny_write_paths*/ &[],
            /*tty*/ false,
            /*stdin_open*/ false,
            /*use_private_desktop*/ true,
        )
        .await
        .expect("spawn legacy non-tty cmd session");
        println!("cmd spawn returned");
        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(10)).await;
        println!("cmd collect returned exit_code={exit_code}");
        let stdout = String::from_utf8_lossy(&stdout);
        assert_eq!(exit_code, 0, "stdout={stdout:?}");
        assert!(stdout.contains("LEGACY-NONTTY-CMD"), "stdout={stdout:?}");
    });
}

#[test]
fn legacy_non_tty_cmd_rejects_deny_read_overrides() {
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-non-tty-deny-read");
        let secret_path =
            AbsolutePathBuf::from_absolute_path(cwd.join("legacy-non-tty-deny-read-secret.env"))
                .expect("absolute deny-read fixture path");
        let permission_profile = PermissionProfile::workspace_write();
        let err = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                "/c".to_string(),
                "echo deny-read".to_string(),
            ],
            cwd.as_path(),
            HashMap::new(),
            /*timeout_ms*/ Some(5_000),
            /*additional_deny_read_paths*/ std::slice::from_ref(&secret_path),
            /*additional_deny_write_paths*/ &[],
            /*tty*/ false,
            /*stdin_open*/ false,
            /*use_private_desktop*/ true,
        )
        .await
        .expect_err("legacy deny-read should require the elevated backend");
        assert!(
            err.to_string()
                .contains("deny-read overrides require the elevated Windows sandbox backend"),
            "unexpected error: {err:#}"
        );
    });
}

#[test]
fn legacy_non_tty_powershell_emits_output() {
    let Some(pwsh) = pwsh_path() else {
        return;
    };
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-non-tty-pwsh");
        println!("pwsh codex_home={}", codex_home.path().display());
        let permission_profile = PermissionProfile::workspace_write();
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                pwsh.display().to_string(),
                "-NoProfile".to_string(),
                "-Command".to_string(),
                "Write-Output LEGACY-NONTTY-DIRECT".to_string(),
            ],
            cwd.as_path(),
            HashMap::new(),
            /*timeout_ms*/ Some(5_000),
            /*additional_deny_read_paths*/ &[],
            /*additional_deny_write_paths*/ &[],
            /*tty*/ false,
            /*stdin_open*/ false,
            /*use_private_desktop*/ true,
        )
        .await
        .expect("spawn legacy non-tty powershell session");
        println!("pwsh spawn returned");
        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(10)).await;
        println!("pwsh collect returned exit_code={exit_code}");
        let stdout = String::from_utf8_lossy(&stdout);
        assert_eq!(exit_code, 0, "stdout={stdout:?}");
        assert!(stdout.contains("LEGACY-NONTTY-DIRECT"), "stdout={stdout:?}");
    });
}

#[test]
fn finish_driver_spawn_keeps_stdin_open_when_requested() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let (writer_tx, mut writer_rx) = mpsc::channel::<Vec<u8>>(1);
        let (_stdout_tx, stdout_rx) = broadcast::channel::<Vec<u8>>(1);
        let (exit_tx, exit_rx) = oneshot::channel::<i32>();
        drop(exit_tx);

        let spawned = super::finish_driver_spawn(
            ProcessDriver {
                writer_tx,
                stdout_rx,
                stderr_rx: None,
                exit_rx,
                terminator: None,
                writer_handle: None,
                resizer: None,
            },
            /*stdin_open*/ true,
        );

        spawned
            .session
            .writer_sender()
            .send(b"open".to_vec())
            .await
            .expect("stdin should stay open");
        assert_eq!(writer_rx.recv().await, Some(b"open".to_vec()));
    });
}

#[test]
fn finish_driver_spawn_closes_stdin_when_not_requested() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let (writer_tx, _writer_rx) = mpsc::channel::<Vec<u8>>(1);
        let (_stdout_tx, stdout_rx) = broadcast::channel::<Vec<u8>>(1);
        let (exit_tx, exit_rx) = oneshot::channel::<i32>();
        drop(exit_tx);

        let spawned = super::finish_driver_spawn(
            ProcessDriver {
                writer_tx,
                stdout_rx,
                stderr_rx: None,
                exit_rx,
                terminator: None,
                writer_handle: None,
                resizer: None,
            },
            /*stdin_open*/ false,
        );

        assert!(
            spawned
                .session
                .writer_sender()
                .send(b"closed".to_vec())
                .await
                .is_err(),
            "stdin should be closed when streaming input is disabled"
        );
    });
}

#[test]
fn runner_stdin_writer_sends_close_stdin_after_input_eof() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let tempdir = TempDir::new().expect("create tempdir");
        let frames_path = tempdir.path().join("runner-stdin-frames.bin");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&frames_path)
            .expect("create frame file");
        let outbound_tx = super::start_runner_pipe_writer(file);
        let (writer_tx, writer_rx) = mpsc::channel::<Vec<u8>>(1);
        let writer_handle = super::start_runner_stdin_writer(
            writer_rx,
            outbound_tx,
            /*normalize_newlines*/ false,
            /*stdin_open*/ true,
        );

        writer_tx
            .send(b"hello".to_vec())
            .await
            .expect("send stdin bytes");
        drop(writer_tx);
        writer_handle.await.expect("join stdin writer");

        let frames = wait_for_frame_count(&frames_path, /*expected_frames*/ 2);

        match &frames[0] {
            Message::Stdin { payload } => {
                let bytes = decode_bytes(&payload.data_b64).expect("decode stdin payload");
                assert_eq!(bytes, b"hello".to_vec());
            }
            other => panic!("expected stdin frame, got {other:?}"),
        }

        match &frames[1] {
            Message::CloseStdin { .. } => {}
            other => panic!("expected close-stdin frame, got {other:?}"),
        }
    });
}

#[test]
fn runner_resizer_sends_resize_frame() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let tempdir = TempDir::new().expect("create tempdir");
        let frames_path = tempdir.path().join("runner-resize-frames.bin");
        let file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&frames_path)
            .expect("create frame file");
        let outbound_tx = super::start_runner_pipe_writer(file);
        let mut resizer = super::make_runner_resizer(outbound_tx);

        resizer(codex_utils_pty::TerminalSize {
            rows: 45,
            cols: 132,
        })
        .expect("send resize frame");

        let frames = wait_for_frame_count(&frames_path, /*expected_frames*/ 1);
        match &frames[0] {
            Message::Resize { payload } => {
                assert_eq!(payload.rows, 45);
                assert_eq!(payload.cols, 132);
            }
            other => panic!("expected resize frame, got {other:?}"),
        }
    });
}

#[test]
fn legacy_capture_powershell_emits_output() {
    let Some(pwsh) = pwsh_path() else {
        return;
    };
    let _guard = legacy_process_test_guard();
    let cwd = sandbox_cwd();
    let codex_home = sandbox_home("legacy-capture-pwsh");
    println!("capture pwsh codex_home={}", codex_home.path().display());
    let permission_profile = PermissionProfile::workspace_write();
    let result = run_windows_sandbox_capture(
        &permission_profile,
        workspace_roots_for(cwd.as_path()).as_slice(),
        codex_home.path(),
        vec![
            pwsh.display().to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Write-Output LEGACY-CAPTURE-DIRECT".to_string(),
        ],
        cwd.as_path(),
        HashMap::new(),
        Some(10_000),
        /*cancellation*/ None,
        /*use_private_desktop*/ true,
    )
    .expect("run legacy capture powershell");
    println!("capture pwsh exit_code={}", result.exit_code);
    println!("capture pwsh timed_out={}", result.timed_out);
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    println!("capture pwsh stderr={stderr:?}");
    assert_eq!(result.exit_code, 0, "stdout={stdout:?} stderr={stderr:?}");
    assert!(
        stdout.contains("LEGACY-CAPTURE-DIRECT"),
        "stdout={stdout:?}"
    );
}

#[test]
fn legacy_write_restricted_deletion_limitation_is_explicit() {
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let (test_root, mut current_user_sid) =
            declared_windows_test_root("legacy-delete-writable-roots");
        let codex_home = sandbox_home("legacy-delete-writable-roots");
        let workspace = test_root.path().join("workspace");
        let temp_root = test_root.path().join("temp");
        let tmp_root = test_root.path().join("tmp");
        let outside_root = test_root.path().join("outside");
        for directory in [&workspace, &temp_root, &tmp_root, &outside_root] {
            fs::create_dir_all(directory).expect("create legacy delete test directory");
        }
        let protected_git_dir = workspace.join(".git");
        fs::create_dir(&protected_git_dir).expect("create protected .git directory");

        let workspace_file = workspace.join("workspace-delete.txt");
        let temp_file = temp_root.join("temp-delete.txt");
        let tmp_file = tmp_root.join("tmp-delete.txt");
        let outside_file = outside_root.join("outside-delete.txt");
        fs::write(&workspace_file, "workspace").expect("seed workspace file");
        fs::write(&temp_file, "temp").expect("seed TEMP file");
        fs::write(&tmp_file, "tmp").expect("seed TMP file");
        fs::write(&outside_file, "outside").expect("seed outside file");

        let script = workspace.join("delete-fixtures.cmd");
        fs::write(
            &script,
            concat!(
                "@echo off\r\n",
                "del /f /q \"%WORKSPACE_DELETE%\"\r\n",
                "del /f /q \"%TEMP_DELETE%\"\r\n",
                "del /f /q \"%TMP_DELETE%\"\r\n",
                "del /f /q \"%OUTSIDE_DELETE%\"\r\n",
                "rmdir \"%PROTECTED_GIT_DIR%\"\r\n",
                "exit /b 0\r\n",
            ),
        )
        .expect("write delete script");

        let env_map = HashMap::from([
            ("TEMP".to_string(), temp_root.to_string_lossy().into_owned()),
            ("TMP".to_string(), tmp_root.to_string_lossy().into_owned()),
            (
                "WORKSPACE_DELETE".to_string(),
                workspace_file.to_string_lossy().into_owned(),
            ),
            (
                "TEMP_DELETE".to_string(),
                temp_file.to_string_lossy().into_owned(),
            ),
            (
                "TMP_DELETE".to_string(),
                tmp_file.to_string_lossy().into_owned(),
            ),
            (
                "OUTSIDE_DELETE".to_string(),
                outside_file.to_string_lossy().into_owned(),
            ),
            (
                "PROTECTED_GIT_DIR".to_string(),
                protected_git_dir.to_string_lossy().into_owned(),
            ),
        ]);

        let permission_profile = PermissionProfile::workspace_write();
        let permissions =
            ResolvedWindowsSandboxPermissions::try_from_permission_profile_for_workspace_roots(
                &permission_profile,
                workspace_roots_for(workspace.as_path()).as_slice(),
            )
            .expect("resolve legacy delete permissions");
        let capability_roots = legacy_session_capability_roots(
            &permissions,
            workspace.as_path(),
            &env_map,
            codex_home.path(),
        );
        let capability_sids = root_capability_sids(
            codex_home.path(),
            workspace.as_path(),
            capability_roots,
        )
        .expect("resolve legacy delete capability SIDs");
        let user_sid = current_user_sid.as_mut_ptr() as *mut c_void;
        assert_eq!(
            (
                path_mask_allows(
                    test_root.path(),
                    &[user_sid],
                    FILE_ALL_ACCESS,
                    /*require_all_bits*/ true,
                )
                .expect("read normalized test root DACL"),
                capability_sids.iter().all(|capability| {
                    !path_mask_allows(
                        &outside_file,
                        &[capability.sid.as_ptr()],
                        DELETE,
                        /*require_all_bits*/ false,
                    )
                    .expect("read outside file DACL")
                }),
                capability_sids.iter().all(|capability| {
                    !path_mask_allows(
                        &outside_root,
                        &[capability.sid.as_ptr()],
                        FILE_DELETE_CHILD,
                        /*require_all_bits*/ false,
                    )
                    .expect("read outside directory DACL")
                }),
            ),
            (true, true, true),
            "test fixture must grant its owner full control without granting deletion authority to sandbox capability SIDs"
        );
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(workspace.as_path()).as_slice(),
            codex_home.path(),
            vec![
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                "/d".to_string(),
                "/c".to_string(),
                script.display().to_string(),
            ],
            workspace.as_path(),
            env_map,
            /*timeout_ms*/ Some(5_000),
            &[],
            &[],
            /*tty*/ false,
            /*stdin_open*/ false,
            /*use_private_desktop*/ true,
        )
        .await
        .expect("spawn legacy delete session");
        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(/*secs*/ 10))
                .await;
        let stdout = String::from_utf8_lossy(&stdout);

        // WRITE_RESTRICTED does not apply restricting SIDs to standalone
        // DELETE/FILE_DELETE_CHILD checks. Keep this normalized fixture as a
        // characterization until launch dependencies have explicit capability
        // read access and the legacy token can safely use full restriction.
        assert_eq!(
            (
                exit_code,
                workspace_file.exists(),
                temp_file.exists(),
                tmp_file.exists(),
                fs::read_to_string(&outside_file).ok(),
                protected_git_dir.is_dir(),
            ),
            (0, false, false, false, None, false),
            "stdout={stdout:?}\n{}",
            sandbox_log(codex_home.path())
        );
    });
}

#[test]
fn legacy_capture_cancellation_is_not_reported_as_timeout() {
    let Some(pwsh) = pwsh_path() else {
        eprintln!("skipping cancellation regression test: PowerShell 7 is not installed");
        return;
    };
    let _guard = legacy_process_test_guard();
    let cwd = sandbox_cwd();
    let codex_home = sandbox_home("legacy-capture-cancel");
    let permission_profile = PermissionProfile::workspace_write();
    let cancelled = Arc::new(AtomicBool::new(false));
    let cancelled_for_token = Arc::clone(&cancelled);
    let cancellation =
        WindowsSandboxCancellationToken::new(move || cancelled_for_token.load(Ordering::SeqCst));
    let cancelled_for_thread = Arc::clone(&cancelled);
    let cancel_thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        cancelled_for_thread.store(true, Ordering::SeqCst);
    });

    let started_at = Instant::now();
    let result = run_windows_sandbox_capture(
        &permission_profile,
        workspace_roots_for(cwd.as_path()).as_slice(),
        codex_home.path(),
        vec![
            pwsh.display().to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            "Start-Sleep -Seconds 30".to_string(),
        ],
        cwd.as_path(),
        HashMap::new(),
        Some(30_000),
        /*cancellation*/ Some(cancellation),
        /*use_private_desktop*/ true,
    )
    .expect("run legacy capture powershell with cancellation");
    cancel_thread.join().expect("cancel thread should finish");

    assert!(
        started_at.elapsed() < Duration::from_secs(10),
        "cancellation should end capture before the timeout"
    );
    assert!(
        !result.timed_out,
        "cancellation should not be reported as a timeout"
    );
    assert_ne!(result.exit_code, 0);
}

#[test]
fn legacy_tty_powershell_emits_output_and_accepts_input() {
    let Some(pwsh) = pwsh_path() else {
        return;
    };
    let _guard = legacy_process_test_guard();
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-tty-pwsh");
        println!("tty pwsh codex_home={}", codex_home.path().display());
        let permission_profile = PermissionProfile::workspace_write();
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                pwsh.display().to_string(),
                "-NoLogo".to_string(),
                "-NoProfile".to_string(),
                "-NoExit".to_string(),
                "-Command".to_string(),
                "$PID; Write-Output ready".to_string(),
            ],
            cwd.as_path(),
            HashMap::new(),
            /*timeout_ms*/ Some(10_000),
            /*additional_deny_read_paths*/ &[],
            /*additional_deny_write_paths*/ &[],
            /*tty*/ true,
            /*stdin_open*/ true,
            /*use_private_desktop*/ true,
        )
        .await
        .expect("spawn legacy tty powershell session");
        println!("tty pwsh spawn returned");

        let writer = spawned.session.writer_sender();
        writer
            .send(b"Write-Output second\n".to_vec())
            .await
            .expect("send second command");
        writer
            .send(b"exit\n".to_vec())
            .await
            .expect("send exit command");
        spawned.session.close_stdin();

        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(15)).await;
        let stdout = String::from_utf8_lossy(&stdout);
        assert_eq!(exit_code, 0, "stdout={stdout:?}");
        assert!(stdout.contains("ready"), "stdout={stdout:?}");
        assert!(stdout.contains("second"), "stdout={stdout:?}");
    });
}

#[test]
#[ignore = "TODO: legacy ConPTY cmd.exe exits with STATUS_DLL_INIT_FAILED in CI"]
fn legacy_tty_cmd_emits_output_and_accepts_input() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-tty-cmd");
        println!("tty cmd codex_home={}", codex_home.path().display());
        let permission_profile = PermissionProfile::workspace_write();
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                "/K".to_string(),
                "echo ready".to_string(),
            ],
            cwd.as_path(),
            HashMap::new(),
            /*timeout_ms*/ Some(10_000),
            /*additional_deny_read_paths*/ &[],
            /*additional_deny_write_paths*/ &[],
            /*tty*/ true,
            /*stdin_open*/ true,
            /*use_private_desktop*/ true,
        )
        .await
        .expect("spawn legacy tty cmd session");
        println!("tty cmd spawn returned");

        let writer = spawned.session.writer_sender();
        writer
            .send(b"echo second\n".to_vec())
            .await
            .expect("send second command");
        writer
            .send(b"exit\n".to_vec())
            .await
            .expect("send exit command");
        spawned.session.close_stdin();

        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(15)).await;
        let stdout = String::from_utf8_lossy(&stdout);
        assert_eq!(exit_code, 0, "stdout={stdout:?}");
        assert!(stdout.contains("ready"), "stdout={stdout:?}");
        assert!(stdout.contains("second"), "stdout={stdout:?}");
    });
}

#[test]
#[ignore = "TODO: legacy ConPTY cmd.exe exits with STATUS_DLL_INIT_FAILED in CI"]
fn legacy_tty_cmd_default_desktop_emits_output_and_accepts_input() {
    let runtime = current_thread_runtime();
    runtime.block_on(async move {
        let cwd = sandbox_cwd();
        let codex_home = sandbox_home("legacy-tty-cmd-default-desktop");
        println!(
            "tty cmd default desktop codex_home={}",
            codex_home.path().display()
        );
        let permission_profile = PermissionProfile::workspace_write();
        let spawned = spawn_windows_sandbox_session_legacy(
            &permission_profile,
            workspace_roots_for(cwd.as_path()).as_slice(),
            codex_home.path(),
            vec![
                "C:\\Windows\\System32\\cmd.exe".to_string(),
                "/K".to_string(),
                "echo ready".to_string(),
            ],
            cwd.as_path(),
            HashMap::new(),
            /*timeout_ms*/ Some(10_000),
            /*additional_deny_read_paths*/ &[],
            /*additional_deny_write_paths*/ &[],
            /*tty*/ true,
            /*stdin_open*/ true,
            /*use_private_desktop*/ false,
        )
        .await
        .expect("spawn legacy tty cmd session");
        println!("tty cmd default desktop spawn returned");

        let writer = spawned.session.writer_sender();
        writer
            .send(b"echo second\n".to_vec())
            .await
            .expect("send second command");
        writer
            .send(b"exit\n".to_vec())
            .await
            .expect("send exit command");
        spawned.session.close_stdin();

        let (stdout, exit_code) =
            collect_stdout_and_exit(spawned, codex_home.path(), Duration::from_secs(15)).await;
        let stdout = String::from_utf8_lossy(&stdout);
        assert_eq!(exit_code, 0, "stdout={stdout:?}");
        assert!(stdout.contains("ready"), "stdout={stdout:?}");
        assert!(stdout.contains("second"), "stdout={stdout:?}");
    });
}
