//! Installs updates, validates the server restart, then transfers updater ownership.

use std::path::Path;
#[cfg(unix)]
use std::process::Command as StdCommand;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::RouteAwareClientPool;
use futures::FutureExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
#[cfg(unix)]
use tokio::signal::unix::Signal;
#[cfg(unix)]
use tokio::signal::unix::SignalKind;
#[cfg(unix)]
use tokio::signal::unix::signal;
use tokio::time::sleep;

use crate::Daemon;
use crate::RestartIfRunningOutcome;
use crate::RestartMode;
use crate::UpdaterRefreshMode;
use crate::managed_install::ExecutableIdentity;
use crate::managed_install::executable_identity;
use crate::managed_install::resolved_managed_codex_bin;

const INITIAL_UPDATE_DELAY: Duration = Duration::from_secs(5 * 60);
const RESTART_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const UPDATE_INTERVAL: Duration = Duration::from_secs(60 * 60);
#[cfg(unix)]
const INSTALL_URL: &str = "https://chatgpt.com/codex/install.sh";
#[cfg(windows)]
const INSTALL_URL: &str = "https://chatgpt.com/codex/install.ps1";

pub(crate) async fn run(http_client_factory: HttpClientFactory) -> Result<()> {
    #[cfg(unix)]
    let mut terminate =
        signal(SignalKind::terminate()).context("failed to install updater shutdown handler")?;
    #[cfg(windows)]
    let updater = {
        let daemon = Daemon::from_environment()?;
        crate::backend::pid_update_loop_backend(
            daemon.backend_paths(&daemon.load_settings().await?),
        )
    };
    #[cfg(windows)]
    updater.wait_for_ownership().await?;
    #[cfg(windows)]
    let mut terminate = Signal;
    #[cfg(windows)]
    let _installer_job = crate::backend::windows::updater_job()?;
    let running_updater_identity = current_updater_identity().await?;
    #[cfg(windows)]
    updater.mark_ready().await?;
    let http = RouteAwareClientPool::new_without_request_logging(
        http_client_factory,
        ClientRouteClass::Other,
    );
    if sleep_or_terminate(INITIAL_UPDATE_DELAY, &mut terminate).await {
        return Ok(());
    }
    loop {
        // Failed successor cleanup leaves its PID published. The predecessor
        // must stop instead of installing again without ownership.
        #[cfg(windows)]
        updater.wait_for_ownership().await?;
        match update_once(&http, &running_updater_identity, &mut terminate).await {
            Ok(UpdateLoopControl::Continue) | Err(_) => {}
            Ok(UpdateLoopControl::Stop) => return Ok(()),
        }
        if sleep_or_terminate(UPDATE_INTERVAL, &mut terminate).await {
            return Ok(());
        }
    }
}

async fn sleep_or_terminate(duration: Duration, terminate: &mut Signal) -> bool {
    tokio::select! {
        _ = sleep(duration) => false,
        _ = terminate.recv() => true,
    }
}

enum UpdateLoopControl {
    Continue,
    Stop,
}

async fn update_once(
    http: &RouteAwareClientPool,
    running_updater_identity: &ExecutableIdentity,
    terminate: &mut Signal,
) -> Result<UpdateLoopControl> {
    let daemon = Daemon::from_environment()?;
    if !daemon.is_stable_standalone_release()? {
        // An installer can be between changing current and publishing its
        // latest-channel marker. Retry after the interval instead of exiting.
        return Ok(UpdateLoopControl::Continue);
    }
    let codex_home = daemon
        .settings_file
        .parent()
        .and_then(Path::parent)
        .context("daemon settings path has no Codex home")?;
    let current = std::fs::canonicalize(codex_home.join("packages/standalone/current"))?;
    let previous_release = current
        .file_name()
        .context("managed release has no name")?
        .to_string_lossy()
        .into_owned();
    let script = tokio::select! {
        result = fetch_installer_script(http) => result?,
        _ = terminate.recv() => return Ok(UpdateLoopControl::Stop),
    };
    anyhow::ensure!(
        script
            .windows(b"CODEX_INSTALL_IF_LATEST".len())
            .any(|window| window == b"CODEX_INSTALL_IF_LATEST"),
        "standalone installer does not support guarded updates"
    );
    if !daemon.is_stable_standalone_release()? {
        return Ok(UpdateLoopControl::Continue);
    }
    #[cfg(unix)]
    if matches!(
        run_installer_script(&script, &previous_release, codex_home, terminate.recv()).await?,
        UpdateLoopControl::Stop
    ) {
        return Ok(UpdateLoopControl::Stop);
    }
    #[cfg(windows)]
    tokio::select! {
        result = run_installer_script(&script, &previous_release) => { result?; },
        _ = terminate.recv() => return Ok(UpdateLoopControl::Stop),
    }
    if !daemon.is_stable_standalone_release()? {
        return Ok(UpdateLoopControl::Continue);
    }

    let managed_codex_bin = resolved_managed_codex_bin(&daemon.managed_codex_bin).await?;
    let managed_identity = executable_identity(&managed_codex_bin).await?;
    let (restart_mode, updater_refresh_mode) =
        update_modes_for_identities(running_updater_identity, &managed_identity);

    loop {
        if terminate.recv().now_or_never().flatten().is_some() {
            return Ok(UpdateLoopControl::Stop);
        }
        match daemon
            .try_restart_if_running(restart_mode, updater_refresh_mode, &managed_codex_bin)
            .await?
        {
            RestartIfRunningOutcome::Busy => {
                if sleep_or_terminate(RESTART_RETRY_INTERVAL, terminate).await {
                    return Ok(UpdateLoopControl::Stop);
                }
            }
            RestartIfRunningOutcome::Restarted => {
                #[cfg(windows)]
                if updater_refresh_mode == UpdaterRefreshMode::ReexecIfManagedBinaryChanged {
                    return Ok(UpdateLoopControl::Stop);
                }
                return Ok(UpdateLoopControl::Continue);
            }
            RestartIfRunningOutcome::NotRunning
            | RestartIfRunningOutcome::NotReady
            | RestartIfRunningOutcome::AlreadyCurrent => {
                return Ok(if daemon.is_stable_standalone_release()? {
                    UpdateLoopControl::Continue
                } else {
                    UpdateLoopControl::Stop
                });
            }
        }
    }
}

async fn current_updater_identity() -> Result<ExecutableIdentity> {
    let current_exe =
        std::env::current_exe().context("failed to resolve current updater executable")?;
    executable_identity(&current_exe).await
}

fn update_modes_for_identities(
    running_updater_identity: &ExecutableIdentity,
    managed_identity: &ExecutableIdentity,
) -> (RestartMode, UpdaterRefreshMode) {
    if running_updater_identity == managed_identity {
        (RestartMode::IfVersionChanged, UpdaterRefreshMode::None)
    } else {
        (
            RestartMode::Always,
            UpdaterRefreshMode::ReexecIfManagedBinaryChanged,
        )
    }
}

#[cfg(unix)]
pub(crate) fn reexec_managed_updater(managed_codex_bin: &std::path::Path) -> Result<()> {
    let err = StdCommand::new(managed_codex_bin)
        .args(["app-server", "daemon", "pid-update-loop"])
        .exec();
    Err(err).with_context(|| {
        format!(
            "failed to replace updater with managed Codex binary {}",
            managed_codex_bin.display()
        )
    })
}

async fn run_installer_script(
    script: &[u8],
    previous_release: &str,
    #[cfg(unix)] codex_home: &Path,
    #[cfg(unix)] terminate: impl std::future::Future<Output = Option<()>>,
) -> Result<UpdateLoopControl> {
    #[cfg(unix)]
    let mut command = {
        let mut command = Command::new("/bin/sh");
        command.arg("-s");
        command.process_group(0);
        command
    };
    #[cfg(windows)]
    let mut command = {
        let mut command = Command::new("powershell.exe");
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", "try { Invoke-Expression ([Console]::In.ReadToEnd()) } catch { Write-Error $_; exit 1 }"])
            .env("CODEX_NON_INTERACTIVE", "1")
            .kill_on_drop(true);
        command
    };
    let mut child = command
        .env("CODEX_RELEASE", "latest")
        .env("CODEX_INSTALL_IF_LATEST", "1")
        .env("CODEX_UPDATE_FROM_RELEASE", previous_release)
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to invoke standalone Codex updater")?;
    let mut stdin = child
        .stdin
        .take()
        .context("standalone Codex updater stdin was unavailable")?;
    #[cfg(unix)]
    let mut terminate = std::pin::pin!(terminate);
    #[cfg(unix)]
    let write_result = tokio::select! {
        result = stdin.write_all(script) => Some(result),
        _ = &mut terminate => None,
    };
    #[cfg(windows)]
    let write_result = Some(stdin.write_all(script).await);
    drop(stdin);
    #[cfg(unix)]
    if write_result.is_none() {
        cancel_installer(&mut child, codex_home).await;
        return Ok(UpdateLoopControl::Stop);
    }
    write_result
        .context("installer write was cancelled")?
        .context("failed to pass standalone Codex updater to shell")?;
    #[cfg(unix)]
    let status = tokio::select! {
        result = child.wait() => result,
        _ = &mut terminate => {
            cancel_installer(&mut child, codex_home).await;
            return Ok(UpdateLoopControl::Stop);
        }
    };
    #[cfg(windows)]
    let status = child.wait().await;
    let status = status.context("failed to wait for standalone Codex updater")?;

    if status.success() {
        Ok(UpdateLoopControl::Continue)
    } else {
        anyhow::bail!("standalone Codex updater exited with status {status}")
    }
}

#[cfg(unix)]
async fn cancel_installer(child: &mut tokio::process::Child, codex_home: &Path) {
    let Some(pid) = child.id().and_then(|pid| libc::pid_t::try_from(pid).ok()) else {
        return;
    };
    // Let the shell's EXIT/TERM trap release the installer lock first.
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    sleep(Duration::from_secs(2)).await;
    // Keep the shell unreaped until after the group kill, so its PID cannot
    // be reused while descendants that ignored TERM are still running.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    // A forced kill can bypass the shell trap on hosts using the mkdir lock.
    // The lock is ours only if its recorded owner is this still-unreaped shell.
    let lock = codex_home.join("packages/standalone/install.lock.d");
    if std::fs::read_to_string(lock.join("pid")).is_ok_and(|owner| owner.trim() == pid.to_string())
    {
        let _ = std::fs::remove_dir_all(lock);
    }
    let _ = child.wait().await;
}

async fn fetch_installer_script(http: &impl InstallerHttp) -> Result<Vec<u8>> {
    match http.get(INSTALL_URL).await? {
        InstallerResponse::Success(body) => Ok(body),
        InstallerResponse::Unsuccessful { status } => {
            anyhow::bail!("standalone Codex updater request failed with status {status}")
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum InstallerResponse {
    Success(Vec<u8>),
    Unsuccessful { status: u16 },
}

/// HTTP boundary used to download the standalone installer.
///
/// Implementations must issue a GET for the supplied URL, return exact response bytes for a
/// successful status, and report a non-success status without buffering its response body.
trait InstallerHttp: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a str,
    ) -> impl std::future::Future<Output = Result<InstallerResponse>> + Send + 'a;
}

impl InstallerHttp for RouteAwareClientPool {
    async fn get(&self, url: &str) -> Result<InstallerResponse> {
        let response = RouteAwareClientPool::get(self, url)
            .send()
            .await
            .context("failed to fetch standalone Codex updater")?;
        if !response.status().is_success() {
            return Ok(InstallerResponse::Unsuccessful {
                status: response.status().as_u16(),
            });
        }
        let body = response
            .bytes()
            .await
            .context("failed to read standalone Codex updater")?
            .to_vec();
        Ok(InstallerResponse::Success(body))
    }
}

#[cfg(test)]
#[path = "update_loop_tests.rs"]
mod tests;

#[cfg(windows)]
struct Signal;

#[cfg(windows)]
impl Signal {
    async fn recv(&mut self) -> Option<()> {
        // An unreadable control path must stop the updater rather than disable shutdown.
        let _ = codex_app_server_transport::daemon_shutdown_signal().await;
        Some(())
    }
}
