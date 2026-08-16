use std::sync::Mutex;

use pretty_assertions::assert_eq;

use super::INSTALL_URL;
use super::InstallerHttp;
use super::InstallerResponse;
use super::SEDNA_STANDALONE_INSTALLER_URL;
use super::fetch_installer_script;
use super::fetch_installer_script_from_url;
#[cfg(unix)]
use super::install_latest_sedna_standalone;
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

#[tokio::test]
async fn sedna_installer_fetch_uses_exact_url_and_preserves_bytes() {
    let script = b"#!/bin/bash\nprintf 'sedna update bytes'\n".to_vec();
    let http = FakeInstallerHttp::new(InstallerResponse::Success(script.clone()));

    assert_eq!(
        fetch_installer_script_from_url(&http, SEDNA_STANDALONE_INSTALLER_URL)
            .await
            .expect("Sedna installer fetch should succeed"),
        script
    );
    assert_eq!(
        http.requested_urls(),
        vec![SEDNA_STANDALONE_INSTALLER_URL.to_string()]
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sedna_updater_executes_stale_client_argument_contract() {
    let script = br#"#!/usr/bin/env bash
set -euo pipefail
expected=(--repository sednalabs/codex --release-tag latest --allow-prerelease)
[[ "$#" -eq "${#expected[@]}" ]]
for expected_arg in "${expected[@]}"; do
  [[ "$1" == "$expected_arg" ]]
  shift
done
printf 'fixture updater diagnostic\n' >&2
"#
    .to_vec();
    let http = FakeInstallerHttp::new(InstallerResponse::Success(script));

    install_latest_sedna_standalone(&http)
        .await
        .expect("Sedna updater should execute the legacy argument contract");
    assert_eq!(
        http.requested_urls(),
        vec![SEDNA_STANDALONE_INSTALLER_URL.to_string()]
    );
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
