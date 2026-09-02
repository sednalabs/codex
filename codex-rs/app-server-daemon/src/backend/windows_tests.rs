use super::Process;
use pretty_assertions::assert_eq;
use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
use windows_sys::Win32::System::Threading::TerminateProcess;

#[tokio::test]
async fn identity_queries_do_not_require_termination_access() {
    let mut child = tokio::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep 60",
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true)
        .spawn()
        .expect("child");
    let process = Process::open(child.id().expect("pid"))
        .expect("query handle")
        .expect("live process");
    assert!(!process.start_time().expect("creation time").is_empty());
    assert!(process.is_running().expect("liveness"));
    // Check the rights on the actual query handle, independent of privileges
    // that could let the caller reopen the process with termination access.
    assert_eq!(
        unsafe {
            TerminateProcess(process.0.as_raw_handle() as _, /*uexitcode*/ 1)
        },
        0
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(ERROR_ACCESS_DENIED as i32)
    );
    assert!(
        process
            .is_running()
            .expect("query must not terminate child")
    );
    child
        .kill()
        .await
        .expect("cleanup through original spawn handle");
}
