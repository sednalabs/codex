#![cfg(target_os = "windows")]

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use pretty_assertions::assert_eq;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;
use tempfile::TempDir;

static WINDOWS_PROCESS_TEST_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn windows_process_test_guard() -> MutexGuard<'static, ()> {
    WINDOWS_PROCESS_TEST_LOCK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

pub(super) fn windows_powershell_path() -> PathBuf {
    let system_root = std::env::var_os("SystemRoot").expect("SystemRoot should be set on Windows");
    let path = PathBuf::from(system_root).join("System32\\WindowsPowerShell\\v1.0\\powershell.exe");
    assert!(
        path.is_file(),
        "Windows PowerShell is required for job lifecycle tests: {}",
        path.display()
    );
    path
}

fn powershell_single_quoted(value: &Path) -> String {
    value.display().to_string().replace('\'', "''")
}

fn powershell_encoded_command(script: &str) -> String {
    let bytes = script
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    BASE64_STANDARD.encode(bytes)
}

pub(super) struct GrandchildFixture {
    _test_dir: TempDir,
    ticks_path: PathBuf,
    root_ready_path: PathBuf,
    grandchild_ready_path: PathBuf,
    pub(super) command: Vec<String>,
}

#[derive(Debug)]
pub(super) struct GrandchildReadiness {
    pub(super) root_ready: bool,
    pub(super) grandchild_ready: bool,
    pub(super) ticks: u64,
}

impl GrandchildReadiness {
    pub(super) fn is_complete(&self) -> bool {
        self.root_ready && self.grandchild_ready && self.ticks >= 3
    }
}

impl GrandchildFixture {
    pub(super) fn readiness(&self) -> GrandchildReadiness {
        GrandchildReadiness {
            root_ready: self.root_ready_path.exists(),
            grandchild_ready: self.grandchild_ready_path.exists(),
            ticks: fs::metadata(&self.ticks_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0),
        }
    }
}

pub(super) fn grandchild_fixture(
    cwd: &Path,
    powershell: &Path,
    root_tail: &str,
) -> GrandchildFixture {
    let test_dir = tempfile::tempdir_in(cwd).expect("create grandchild test directory");
    let ticks_path = test_dir.path().join("ticks.txt");
    let root_ready_path = test_dir.path().join("root-ready.txt");
    let grandchild_ready_path = test_dir.path().join("grandchild-ready.txt");
    let child_script = format!(
        "[IO.File]::WriteAllText('{}', 'ready'); while ($true) {{ [IO.File]::AppendAllText('{}', 'x'); Start-Sleep -Milliseconds 25 }}",
        powershell_single_quoted(&grandchild_ready_path),
        powershell_single_quoted(&ticks_path)
    );
    let root_script = format!(
        "[IO.File]::WriteAllText('{}', [string]$PID); $child = Start-Process -PassThru -FilePath '{}' -ArgumentList @('-NoProfile', '-EncodedCommand', '{}'); {root_tail}",
        powershell_single_quoted(&root_ready_path),
        powershell_single_quoted(powershell),
        powershell_encoded_command(&child_script),
    );
    GrandchildFixture {
        _test_dir: test_dir,
        ticks_path,
        root_ready_path,
        grandchild_ready_path,
        command: vec![
            powershell.display().to_string(),
            "-NoProfile".to_string(),
            "-EncodedCommand".to_string(),
            powershell_encoded_command(&root_script),
        ],
    }
}

pub(super) fn assert_grandchild_stopped(fixture: &GrandchildFixture) {
    let length_after_exit = fs::metadata(&fixture.ticks_path)
        .expect("ticks file after root exit")
        .len();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        fs::metadata(&fixture.ticks_path)
            .expect("ticks file after stability wait")
            .len(),
        length_after_exit,
        "grandchild kept writing after the session job closed"
    );
}

#[derive(Clone, Copy)]
pub(super) enum SessionMode {
    Pipe,
    Tty,
}

impl SessionMode {
    pub(super) fn tty(self) -> bool {
        matches!(self, Self::Tty)
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Pipe => "pipe",
            Self::Tty => "tty",
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum SessionEnding {
    ExplicitTermination,
    RootExit,
}

impl SessionEnding {
    pub(super) fn root_tail(self) -> &'static str {
        match self {
            Self::ExplicitTermination => "Start-Sleep -Seconds 30",
            Self::RootExit => "Start-Sleep -Milliseconds 500",
        }
    }
}
