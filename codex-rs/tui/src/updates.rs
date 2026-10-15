#![cfg(not(debug_assertions))]

use crate::legacy_core::config::Config;
use crate::update_action;
use crate::update_action::UpdateAction;
use crate::update_versions::extract_version_from_latest_tag;
use crate::update_versions::is_source_build_version;
use crate::updates_cache::VersionInfo;
use crate::updates_cache::read_version_info;
use crate::updates_cache::version_filepath;
use chrono::Duration;
use chrono::Utc;
use codex_http_client::ClientRouteClass;
use codex_http_client::HttpClientFactory;
use codex_http_client::RouteAwareClientPool;
use codex_install_context::InstallContext;
use codex_login::default_client::default_headers;
use serde::Deserialize;
use std::path::Path;

use crate::version::CODEX_CLI_VERSION;

pub(crate) use crate::updates_cache::dismiss_version;

pub fn get_upgrade_version(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let action = update_action::get_update_action();
    let action = action?;
    let install_context = InstallContext::current();
    let installed_version = install_context
        .sedna_release_receipt()
        .map(|receipt| receipt.release.package_version.to_string())?;
    let version_file = version_filepath(config);
    let info = read_version_info(&version_file).ok();

    if match &info {
        None => true,
        Some(info) => info.last_checked_at < Utc::now() - Duration::hours(20),
    } {
        let http_client_factory = config.http_client_factory();
        // Refresh the cached latest version in the background so TUI startup
        // isn’t blocked by a network call. The UI reads the previously cached
        // value (if any) for this run; the next run shows the banner if needed.
        tokio::spawn(async move {
            check_for_update(&version_file, Some(action), http_client_factory)
                .await
                .inspect_err(|e| tracing::error!("Failed to update version: {e}"))
        });
    }

    info.and_then(|info| {
        if semver::Version::parse(&info.latest_version)
            .ok()
            .zip(semver::Version::parse(&installed_version).ok())
            .is_some_and(|(latest, installed)| latest > installed)
        {
            Some(info.latest_version)
        } else {
            None
        }
    })
}

const SEDNA_LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/sednalabs/codex/releases/latest";
const SEDNA_RELEASES_URL: &str =
    "https://api.github.com/repos/sednalabs/codex/releases?per_page=100";

#[derive(Deserialize, Debug, Clone)]
struct SednaReleaseInfo {
    tag_name: String,
    prerelease: bool,
}

fn sedna_release_version(release: &SednaReleaseInfo) -> Option<semver::Version> {
    let version = release.tag_name.strip_prefix("rust-v")?;
    let version = semver::Version::parse(version).ok()?;
    (release.prerelease == !version.pre.is_empty()).then_some(version)
}

fn newest_sedna_release(releases: &[SednaReleaseInfo]) -> Option<&SednaReleaseInfo> {
    releases
        .iter()
        .filter(|release| sedna_release_version(release).is_some())
        .max_by_key(|release| sedna_release_version(release))
}

async fn check_for_update(
    version_file: &Path,
    action: Option<UpdateAction>,
    http_client_factory: HttpClientFactory,
) -> anyhow::Result<()> {
    let client_pool = RouteAwareClientPool::with_chatgpt_cloudflare_cookies(
        http_client_factory,
        ClientRouteClass::Other,
    )
    .with_legacy_custom_ca_fallback();
    let latest_version = match action {
        Some(UpdateAction::Daemon(_)) => return Ok(()),
        Some(UpdateAction::SednaStandalone) => {
            let receipt = InstallContext::current()
                .sedna_release_receipt()
                .context("installed package is not eligible for Sedna updates")?;
            let url = if receipt.selection_channel == "prerelease" {
                SEDNA_RELEASES_URL
            } else {
                SEDNA_LATEST_RELEASE_URL
            };
            let releases = client_pool
                .get(url)
                .headers(default_headers())
                .send()
                .await?
                .error_for_status()?;
            let latest_tag = if receipt.selection_channel == "prerelease" {
                let releases = releases.json::<Vec<SednaReleaseInfo>>().await?;
                newest_sedna_release(&releases)
                    .context("no Sedna release is currently published")?
                    .tag_name
                    .clone()
            } else {
                let release = releases.json::<SednaReleaseInfo>().await?;
                anyhow::ensure!(
                    !release.prerelease && sedna_release_version(&release).is_some(),
                    "Sedna latest release is not a valid stable release"
                );
                release.tag_name
            };
            extract_version_from_latest_tag(&latest_tag)?
        }
        None => return Ok(()),
    };

    // Preserve any previously dismissed version if present.
    let prev_info = read_version_info(version_file).ok();
    let info = VersionInfo {
        latest_version,
        last_checked_at: Utc::now(),
        dismissed_version: prev_info.and_then(|p| p.dismissed_version),
    };

    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SednaReleaseInfo;
    use super::newest_sedna_release;

    #[test]
    fn prerelease_selection_includes_newer_stable_releases() {
        let releases = [
            SednaReleaseInfo {
                tag_name: "rust-v0.162.0-alpha.2".into(),
                prerelease: true,
            },
            SednaReleaseInfo {
                tag_name: "rust-v0.161.0".into(),
                prerelease: false,
            },
            SednaReleaseInfo {
                tag_name: "rust-v0.162.0".into(),
                prerelease: false,
            },
            SednaReleaseInfo {
                tag_name: "v99.0.0".into(),
                prerelease: true,
            },
        ];
        assert_eq!(
            newest_sedna_release(&releases).map(|release| release.tag_name.as_str()),
            Some("rust-v0.162.0")
        );
    }
}

/// Returns the latest version to show in a popup, if it should be shown.
/// This respects the user's dismissal choice for the current latest version.
pub fn get_upgrade_version_for_popup(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let version_file = version_filepath(config);
    let latest = get_upgrade_version(config)?;
    // If the user dismissed this exact version previously, do not show the popup.
    if let Ok(info) = read_version_info(&version_file)
        && info.dismissed_version.as_deref() == Some(latest.as_str())
    {
        return None;
    }
    Some(latest)
}
