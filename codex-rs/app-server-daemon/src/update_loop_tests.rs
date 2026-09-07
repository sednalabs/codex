use std::sync::Mutex;
#[cfg(unix)]
use std::time::Duration;

use pretty_assertions::assert_eq;

use super::INSTALL_URL;
use super::InstallerHttp;
use super::InstallerResponse;
use super::fetch_installer_script;
use super::update_modes_for_identities;
use crate::RestartMode;
use crate::UpdaterRefreshMode;
use crate::managed_install::executable_identity_from_bytes;

#[test]
fn unchanged_updater_uses_version_based_restart() {
    assert_eq!(
        update_modes_for_identities(
            &executable_identity_from_bytes(b"same"),
            &executable_identity_from_bytes(b"same"),
        ),
        (RestartMode::IfVersionChanged, UpdaterRefreshMode::None)
    );
}

#[test]
fn changed_updater_forces_refresh_even_when_version_may_match() {
    assert_eq!(
        update_modes_for_identities(
            &executable_identity_from_bytes(b"old"),
            &executable_identity_from_bytes(b"new"),
        ),
        (
            RestartMode::Always,
            UpdaterRefreshMode::ReexecIfManagedBinaryChanged,
        )
    );
}

#[tokio::test]
async fn installer_fetch_uses_exact_url_and_preserves_bytes() {
    let script = b"#!/bin/sh\nprintf 'update bytes'\n".to_vec();
    let http = FakeInstallerHttp::new(InstallerResponse::Success(script.clone()));

    assert_eq!(
        fetch_installer_script(&http)
            .await
            .expect("installer fetch should succeed"),
        script
    );
    assert_eq!(http.requested_urls(), vec![INSTALL_URL.to_string()]);
}

#[tokio::test]
async fn installer_fetch_rejects_non_success_status() {
    let http = FakeInstallerHttp::new(InstallerResponse::Unsuccessful { status: 503 });

    let error = fetch_installer_script(&http)
        .await
        .expect_err("non-success response should fail");

    assert!(error.to_string().contains("503"));
    assert_eq!(http.requested_urls(), vec![INSTALL_URL.to_string()]);
}

struct FakeInstallerHttp {
    response: InstallerResponse,
    requested_urls: Mutex<Vec<String>>,
}

impl FakeInstallerHttp {
    fn new(response: InstallerResponse) -> Self {
        Self {
            response,
            requested_urls: Mutex::new(Vec::new()),
        }
    }

    fn requested_urls(&self) -> Vec<String> {
        self.requested_urls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl InstallerHttp for FakeInstallerHttp {
    async fn get(&self, url: &str) -> anyhow::Result<InstallerResponse> {
        self.requested_urls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(url.to_string());
        Ok(self.response.clone())
    }
}

#[cfg(unix)]
#[tokio::test]
async fn cancelling_installer_stops_children_and_releases_fallback_lock() {
    let home = tempfile::TempDir::new().expect("home");
    let ready = home.path().join("ready");
    let delayed = home.path().join("delayed");
    let lock = home.path().join("packages/standalone/install.lock.d");
    let script = format!(
        "mkdir -p '{lock}'\necho $$ > '{lock}/pid'\n(trap '' TERM; echo ready > '{ready}'; sleep 4; echo late > '{delayed}') &\nwait\n",
        lock = lock.display(),
        ready = ready.display(),
        delayed = delayed.display(),
    );
    let (cancel, cancelled) = tokio::sync::oneshot::channel();
    let ready_for_signal = ready.clone();
    let signal_sender = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !ready_for_signal.exists() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "installer did not start"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        cancel.send(()).expect("cancel installer");
    });
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        super::run_installer_script(script.as_bytes(), "0.150.0-test", home.path(), async {
            cancelled.await.ok()
        }),
    )
    .await
    .expect("installer cancellation timed out")
    .expect("installer cancellation failed");
    signal_sender.await.expect("signal sender");
    assert!(matches!(result, super::UpdateLoopControl::Stop));
    assert!(!lock.exists());
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!delayed.exists());
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_installer_is_noninteractive_and_reports_script_failure() {
    let valid = FakeInstallerHttp::new(InstallerResponse::Success(
        br#"
function Test-Installer {
    if ($env:CODEX_NON_INTERACTIVE -ne '1') { throw 'interactive installer' }
}
Test-Installer
"#
        .to_vec(),
    ));
    let script = super::fetch_installer_script(&valid)
        .await
        .expect("fetch installer");
    super::run_installer_script(&script, "0.150.0-x86_64-pc-windows-msvc")
        .await
        .expect("installer succeeds");
    let failing = FakeInstallerHttp::new(InstallerResponse::Success(
        b"throw 'installer failed'".to_vec(),
    ));
    let script = super::fetch_installer_script(&failing)
        .await
        .expect("fetch failing installer");
    assert!(
        super::run_installer_script(&script, "0.150.0-x86_64-pc-windows-msvc")
            .await
            .is_err()
    );
}
