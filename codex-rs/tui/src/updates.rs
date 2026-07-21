#![cfg(not(debug_assertions))]

use crate::legacy_core::config::Config;
use crate::npm_registry;
use crate::npm_registry::NpmPackageInfo;
use crate::update_action;
use crate::update_action::UpdateAction;
use crate::update_versions::extract_version_from_latest_tag;
use crate::update_versions::is_newer;
use crate::update_versions::is_source_build_version;
use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use codex_login::default_client::create_client;
use serde::Deserialize;
use serde::Serialize;
use std::path::Path;
use std::path::PathBuf;

use crate::version::CODEX_CLI_VERSION;
use crate::version::latest_release_api_url;

pub fn get_upgrade_version(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let action = update_action::get_update_action();
    let version_file = version_filepath(config);
    let source = current_version_info_source(action);
    let info = read_version_info(&version_file)
        .ok()
        .filter(|info| source_matches(info, &source));

    if match &info {
        None => true,
        Some(info) => info.last_checked_at < Utc::now() - Duration::hours(20),
    } {
        // Refresh the cached latest version in the background so TUI startup
        // isn’t blocked by a network call. The UI reads the previously cached
        // value (if any) for this run; the next run shows the banner if needed.
        tokio::spawn(async move {
            check_for_update(&version_file, action)
                .await
                .inspect_err(|e| tracing::error!("Failed to update version: {e}"))
        });
    }

    info.and_then(|info| {
        if is_newer(&info.latest_version, CODEX_CLI_VERSION).unwrap_or(false) {
            Some(info.latest_version)
        } else {
            None
        }
    })
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct VersionInfo {
    latest_version: String,
    // ISO-8601 timestamp (RFC3339)
    last_checked_at: DateTime<Utc>,
    #[serde(default)]
    dismissed_version: Option<String>,
    #[serde(default)]
    source: Option<VersionInfoSource>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
struct VersionInfoSource {
    action: Option<String>,
    latest_url: String,
}

const VERSION_FILENAME: &str = "version.json";
// We use the latest version from the cask if installation is via homebrew - homebrew does not immediately pick up the latest release and can lag behind.
const HOMEBREW_CASK_API_URL: &str = "https://formulae.brew.sh/api/cask/codex.json";

#[derive(Deserialize, Debug, Clone)]
struct ReleaseInfo {
    tag_name: String,
}

#[derive(Deserialize, Debug, Clone)]
struct HomebrewCaskInfo {
    version: String,
}

fn version_filepath(config: &Config) -> PathBuf {
    config.codex_home.join(VERSION_FILENAME).into_path_buf()
}

fn read_version_info(version_file: &Path) -> anyhow::Result<VersionInfo> {
    let contents = std::fs::read_to_string(version_file)?;
    Ok(serde_json::from_str(&contents)?)
}

fn current_version_info_source(action: Option<UpdateAction>) -> VersionInfoSource {
    VersionInfoSource {
        action: action.map(|action| action.cache_key().to_string()),
        latest_url: match action {
            Some(UpdateAction::BrewUpgrade) => HOMEBREW_CASK_API_URL.to_string(),
            Some(UpdateAction::NpmGlobalLatest)
            | Some(UpdateAction::BunGlobalLatest)
            | Some(UpdateAction::StandaloneUnix)
            | Some(UpdateAction::StandaloneWindows)
            | None => latest_release_api_url(),
        },
    }
}

fn source_matches(info: &VersionInfo, source: &VersionInfoSource) -> bool {
    info.source.as_ref() == Some(source)
}

async fn check_for_update(version_file: &Path, action: Option<UpdateAction>) -> anyhow::Result<()> {
    let source = current_version_info_source(action);
    let latest_version = match action {
        Some(UpdateAction::BrewUpgrade) => {
            let HomebrewCaskInfo { version } = create_client()
                .get(HOMEBREW_CASK_API_URL)
                .send()
                .await?
                .error_for_status()?
                .json::<HomebrewCaskInfo>()
                .await?;
            version
        }
        Some(UpdateAction::NpmGlobalLatest) | Some(UpdateAction::BunGlobalLatest) => {
            let latest_version = fetch_latest_github_release_version().await?;
            let package_info = create_client()
                .get(npm_registry::PACKAGE_URL)
                .send()
                .await?
                .error_for_status()?
                .json::<NpmPackageInfo>()
                .await?;
            npm_registry::ensure_version_ready(&package_info, &latest_version)?;
            latest_version
        }
        Some(UpdateAction::StandaloneUnix) | Some(UpdateAction::StandaloneWindows) | None => {
            fetch_latest_github_release_version().await?
        }
    };

    // Preserve any previously dismissed version if present.
    let prev_info = read_version_info(version_file).ok();
    let dismissed_version = prev_info
        .filter(|info| source_matches(info, &source))
        .and_then(|p| p.dismissed_version);
    let info = VersionInfo {
        latest_version,
        last_checked_at: Utc::now(),
        dismissed_version,
        source: Some(source),
    };

    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}

async fn fetch_latest_github_release_version() -> anyhow::Result<String> {
    let ReleaseInfo {
        tag_name: latest_tag_name,
    } = create_client()
        .get(latest_release_api_url())
        .send()
        .await?
        .error_for_status()?
        .json::<ReleaseInfo>()
        .await?;
    extract_version_from_latest_tag(&latest_tag_name)
}

/// Returns the latest version to show in a popup, if it should be shown.
/// This respects the user's dismissal choice for the current latest version.
pub fn get_upgrade_version_for_popup(config: &Config) -> Option<String> {
    if !config.check_for_update_on_startup || is_source_build_version(CODEX_CLI_VERSION) {
        return None;
    }

    let version_file = version_filepath(config);
    let source = current_version_info_source(update_action::get_update_action());
    let latest = get_upgrade_version(config)?;
    // If the user dismissed this exact version previously, do not show the popup.
    if let Ok(info) = read_version_info(&version_file)
        && source_matches(&info, &source)
        && info.dismissed_version.as_deref() == Some(latest.as_str())
    {
        return None;
    }
    Some(latest)
}

/// Persist a dismissal for the current latest version so we don't show
/// the update popup again for this version.
pub async fn dismiss_version(config: &Config, version: &str) -> anyhow::Result<()> {
    let version_file = version_filepath(config);
    let mut info = match read_version_info(&version_file) {
        Ok(info) => info,
        Err(_) => return Ok(()),
    };
    info.dismissed_version = Some(version.to_string());
    let json_line = format!("{}\n", serde_json::to_string(&info)?);
    if let Some(parent) = version_file.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(version_file, json_line).await?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_version_from_brew_api_json() {
        //
        // https://formulae.brew.sh/api/cask/codex.json
        let cask_json = r#"{
            "token": "codex",
            "full_token": "codex",
            "tap": "homebrew/cask",
            "version": "0.96.0",
        }"#;
        let HomebrewCaskInfo { version } = serde_json::from_str::<HomebrewCaskInfo>(cask_json)
            .expect("failed to parse version from cask json");
        assert_eq!(version, "0.96.0");
    }
}
