//! Moves a legacy daemon to its dedicated package only during an explicit update.
//! Preparation leaves the legacy selection intact; publishing current is the cutover.

use std::io::Write;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use futures::FutureExt;

use super::InstallerHttp;
use super::InstallerMode;
use super::fetch_installer_script;
use super::run_installer_script;
use super::selected_release;
use crate::Daemon;
use crate::UpdateOutput;
use crate::UpdateStatus;
use crate::backend;
use crate::client;
use crate::install_lock::acquire_install_lock;
use crate::managed_install;

const SEDNA_INSTALL_URL_PREFIX: &str =
    "https://raw.githubusercontent.com/sednalabs/codex/";
const SEDNA_INSTALL_URL_SUFFIX: &str = "/scripts/install/install.sh";

pub(super) async fn run(http: &impl InstallerHttp, legacy: &Daemon) -> Result<UpdateOutput> {
    let _operation_lock = legacy.acquire_operation_lock().await?;
    let previous = selected_release(legacy)?;
    anyhow::ensure!(
        previous.0.ends_with("standalone") && super::manual_update::supported(legacy)?,
        "daemon selection changed; retry the update"
    );
    let home = previous
        .0
        .parent()
        .and_then(Path::parent)
        .context("package root has no Codex home")?;
    let root = home.join("packages/app-server-daemon");
    let legacy_binary = legacy.current_managed_codex_bin()?;
    let resolved_legacy_binary = managed_install::resolved_managed_codex_bin(&legacy_binary).await?;
    anyhow::ensure!(
        resolved_legacy_binary.starts_with(&previous.1),
        "selected daemon binary does not belong to the selected release"
    );
    let legacy_context = codex_install_context::InstallContext::from_exe_in_codex_home(
        false,
        Some(&legacy_binary),
        None,
        home,
    );
    let previous_receipt = legacy_context
        .sedna_release_receipt()
        .context("legacy package is not eligible for Sedna migration; reinstall manually")?;
    let previous_native_version = managed_install::managed_codex_version(&legacy_binary).await?;
    anyhow::ensure!(
        previous_native_version == previous_receipt.release.native_version.to_string(),
        "legacy package native version does not match its Sedna receipt; reinstall manually"
    );
    let expected_target = format!("{}-unknown-linux-musl", std::env::consts::ARCH);
    anyhow::ensure!(
        previous_receipt.release.target == expected_target,
        "legacy package target is not supported for Sedna migration; reinstall manually"
    );
    let settings = legacy.load_settings().await?;
    let running = legacy.running_backend_instance(&settings).await?;
    if running.is_none() && client::probe(&legacy.socket_path).await.is_ok() {
        return super::manual_update::unsupported(legacy).await;
    }
    #[cfg(unix)]
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let cancellation = async {
        #[cfg(unix)]
        tokio::select! {
            _ = terminate.recv() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
        #[cfg(windows)]
        let _ = tokio::signal::ctrl_c().await;
        Some(())
    };
    let mut cancellation = std::pin::pin!(cancellation);

    let installer_url = format!(
        "{SEDNA_INSTALL_URL_PREFIX}{}{SEDNA_INSTALL_URL_SUFFIX}",
        previous_receipt.release.source_commit
    );
    let script = fetch_installer_script(http, &installer_url).await?;
    anyhow::ensure!(
        script
            .windows(b"CODEX_INSTALL_DEFER_SELECTION".len())
            .any(|window| window == b"CODEX_INSTALL_DEFER_SELECTION"),
        "the published installer does not support daemon migration yet; the legacy installation was left unchanged"
    );
    eprintln!("Preparing the daemon update in {}...", root.display());
    #[cfg(unix)]
    {
        if matches!(
            run_installer_script(
                &script,
                InstallerMode::Migration,
                &root,
                &previous_receipt.selection_channel,
                &previous_receipt.release.package_version.to_string(),
                &previous_receipt.release.native_version.to_string(),
                cancellation.as_mut()
            )
            .await?,
            super::UpdateLoopControl::Stop
        ) {
            return Err(std::io::Error::from(std::io::ErrorKind::Interrupted).into());
        }
    }
    #[cfg(windows)]
    tokio::select! {
        result = run_installer_script(
            &script,
            InstallerMode::Migration,
            &root,
            &previous_receipt.selection_channel,
            &previous_receipt.release.package_version.to_string(),
            &previous_receipt.release.native_version.to_string(),
        ) => { result?; },
        _ = &mut cancellation => return Err(std::io::Error::from(std::io::ErrorKind::Interrupted).into()),
    }

    let _install_lock = acquire_install_lock(&root).await?;
    let pending = root.join(".migration-current");
    let release = pending
        .canonicalize()
        .context("installer did not prepare a daemon package")?;
    let name = release
        .file_name()
        .context("prepared release has no name")?;
    let releases = root.join("releases").canonicalize()?;
    anyhow::ensure!(
        release.parent() == Some(releases.as_path())
            && matches!(
                root.join("current").symlink_metadata(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound
            ),
        "installer did not prepare a deferred daemon package"
    );
    let entrypoint = if cfg!(windows) {
        "bin/codex.exe"
    } else {
        "bin/codex"
    };
    let binary = release.join(entrypoint);
    let context = codex_install_context::InstallContext::from_exe_in_codex_home(
        false,
        Some(&binary),
        None,
        home,
    );
    let receipt = context
        .sedna_release_receipt()
        .context("installer did not prepare a verified Sedna package")?;
    anyhow::ensure!(
        receipt.selection_channel == previous_receipt.selection_channel
            && receipt.release.target == expected_target,
        "prepared daemon receipt does not preserve the selected channel and target"
    );
    let version = managed_install::managed_codex_version(&binary).await?;
    anyhow::ensure!(
        version == receipt.release.native_version.to_string(),
        "prepared daemon native version does not match its Sedna receipt"
    );
    let previous_native = semver::Version::parse(&previous_native_version)
        .context("installed daemon native version was invalid")?;
    let prepared_native = semver::Version::parse(&version)
        .context("prepared daemon native version was invalid")?;
    anyhow::ensure!(
        prepared_native >= previous_native,
        "prepared daemon native version is older than the installed daemon"
    );
    anyhow::ensure!(
        name.to_string_lossy()
            == format!("{}-{expected_target}", receipt.release.package_version),
        "prepared daemon package version does not match its release directory"
    );
    // An older production updater understands only the legacy package/PID paths.
    anyhow::ensure!(
        managed_install::supports_daemon_command(
            &binary,
            &["pid-update-loop", "--check-package-ownership"]
        )
        .await,
        "the latest production release does not support daemon migration yet; the legacy daemon was left running"
    );
    #[cfg(windows)]
    backend::windows::ensure_detached_launch(&binary)?;
    anyhow::ensure!(
        selected_release(legacy)? == previous,
        "daemon selection changed; retry the update"
    );
    let marker = root.join("auto-update-version");
    anyhow::ensure!(
        matches!(
            marker.symlink_metadata(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ),
        "daemon release marker already exists; migration was left unchanged"
    );
    let marker_temp = root.join(format!(
        ".auto-update-version.migration-{}",
        std::process::id()
    ));
    let mut marker_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker_temp)
        .context("failed to prepare the daemon release marker")?;
    if let Err(error) = marker_file.write_all(name.to_string_lossy().as_bytes()) {
        let _ = std::fs::remove_file(&marker_temp);
        return Err(error).context("failed to prepare the daemon release marker");
    }
    drop(marker_file);
    if cancellation.as_mut().now_or_never().is_some() {
        let _ = std::fs::remove_file(&marker_temp);
        return Err(std::io::Error::from(std::io::ErrorKind::Interrupted).into());
    }
    let cutover = async {
        backend::pid_update_loop_backend(legacy.backend_paths(&settings))
            .stop()
            .await?;
        anyhow::ensure!(
            selected_release(legacy)? == previous,
            "daemon selection changed; retry the update"
        );
        if let Some(running) = &running {
            crate::thread_recovery::discard_pending(legacy)?;
            running
                .stop_with_grace(settings.shutdown_grace_seconds)
                .await?;
        }
        let current = root.join("current");
        std::fs::rename(&pending, &current)
            .context("failed to select the new daemon package; retry the update")?;
        if let Err(error) = std::fs::rename(&marker_temp, &marker) {
            if let Err(rollback_error) = std::fs::rename(&current, &pending) {
                return Err(error).context(format!(
                    "failed to publish the daemon release marker and failed to roll back current: {rollback_error}"
                ));
            }
            if running.is_some() {
                if let Err(restore_error) = async {
                    legacy.start_managed_backend(&settings).await?;
                    legacy.wait_until_ready().await?;
                    Ok::<(), anyhow::Error>(())
                }
                .await
                {
                    return Err(error).context(format!(
                        "failed to publish the daemon release marker, rolled back current, but failed to restore the legacy daemon: {restore_error}"
                    ));
                }
            }
            return Err(error).context("failed to publish the daemon release marker; selection rolled back");
        }
        Ok(())
    }
    .await;
    if let Err(error) = cutover {
        let _ = std::fs::remove_file(&marker_temp);
        if let Err(restore_error) = async {
            legacy
                .current_installation()?
                .ensure_managed_updater(&settings)
                .await
        }
        .await
        {
            eprintln!(
                "warning: failed to restore the legacy updater after migration failed: {restore_error:#}"
            );
        }
        return Err(error);
    }
    let selected = Daemon {
        pid_file: legacy.pid_file.with_file_name(crate::DAEMON_PID_FILE_NAME),
        update_pid_file: legacy
            .update_pid_file
            .with_file_name(crate::DAEMON_UPDATE_PID_FILE_NAME),
        managed_codex_bin: root.join("current").join(entrypoint),
        ..legacy.clone()
    };
    let running_version = if running.is_some() {
        selected.start_managed_backend(&settings).await.context(
            "daemon migrated but could not start; retry with `codex app-server daemon start`",
        )?;
        Some(
            selected
                .wait_until_ready()
                .await
                .context(
                    "daemon migrated but is not ready; retry with `codex app-server daemon start`",
                )?
                .app_server_version,
        )
    } else {
        None
    };
    if let Err(error) = selected.ensure_managed_updater(&settings).await {
        eprintln!("warning: daemon migrated but its updater could not start: {error:#}");
    }
    Ok(UpdateOutput {
        status: UpdateStatus::Updated,
        installed_version: Some(receipt.release.package_version.to_string()),
        running_version,
        managed_codex_path: selected.managed_codex_bin,
        message: "The daemon was updated and moved to its dedicated package. The legacy CLI package was left unchanged.".to_string(),
    })
}
