//! Diagnoses whether Codex update paths target the running installation.
//!
//! Update diagnostics combine cached version metadata, install-channel hints,
//! and bounded latest-version probes. For npm-managed launches, this module also
//! verifies that npm install -g would update the package root that launched the
//! current process, which catches PATH and prefix mismatches before the user runs
//! an update command.

use std::path::Path;

use codex_core::config::Config;
use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use semver::Version;
use serde::Deserialize;

use super::CheckStatus;
use super::DoctorCheck;
use super::NpmRootCheck;
use super::doctor_install_context;
use super::doctor_managed_by_npm;
use super::npm_global_root_check;
use super::run_command;

const VERSION_FILE_NAME: &str = "version.json";
const CODEX_RELEASE_REPOSITORY: &str = match option_env!("CODEX_RELEASE_REPOSITORY") {
    Some(repository) => repository,
    None => "sednalabs/codex",
};
const CODEX_RELEASE_TAG_PREFIX: &str = match option_env!("CODEX_RELEASE_TAG_PREFIX") {
    Some(prefix) => prefix,
    None => "v",
};
const CODEX_UPDATE_NPM_PACKAGE: &str = match option_env!("CODEX_UPDATE_NPM_PACKAGE") {
    Some(package) => package,
    None => "@openai/codex",
};
const CODEX_UPDATE_BREW_CASK: &str = match option_env!("CODEX_UPDATE_BREW_CASK") {
    Some(cask) => cask,
    None => "codex",
};
const HOMEBREW_CASK_API_URL: &str = "https://formulae.brew.sh/api/cask/codex.json";

/// Builds the update-health row for the current installation.
///
/// Network failures while fetching latest-version metadata degrade the row to a
/// warning instead of failing doctor outright; update freshness is useful
/// support context but should not mask more direct install/config failures.
pub(super) fn updates_check(config: &Config) -> DoctorCheck {
    let current_exe = std::env::current_exe().ok();
    let install_context = doctor_install_context(current_exe.as_deref());
    let mut details = vec![
        format!(
            "check for update on startup: {}",
            config.check_for_update_on_startup
        ),
        format!("update action: {}", update_action_label(&install_context)),
    ];
    let version_file = config.codex_home.join(VERSION_FILE_NAME);
    push_cached_version_details(&mut details, &version_file);

    let mut status = CheckStatus::Ok;
    let mut summary = "update configuration is locally consistent".to_string();
    let mut remediation = None;

    if doctor_managed_by_npm(current_exe.as_deref()) {
        match npm_global_root_check() {
            NpmRootCheck::Match { package_root } => {
                details.push(format!("npm update target: {}", package_root.display()));
            }
            NpmRootCheck::Mismatch {
                running_package_root,
                npm_package_root,
            } => {
                status = CheckStatus::Fail;
                summary = "update would target a different npm install".to_string();
                details.push(format!(
                    "running package root: {}",
                    running_package_root.display()
                ));
                details.push(format!("npm package root: {}", npm_package_root.display()));
                remediation = Some(format!(
                    "Fix PATH or npm prefix so the running package root ({}) matches the npm global package root ({}).",
                    running_package_root.display(),
                    npm_package_root.display()
                ));
            }
            NpmRootCheck::MissingPackageRoot => {
                status = status.max(CheckStatus::Warning);
                summary = "npm update target could not be proven".to_string();
                remediation = Some(
                    "Reinstall or update Codex so the JS shim provides CODEX_MANAGED_PACKAGE_ROOT."
                        .to_string(),
                );
            }
            NpmRootCheck::NpmUnavailable(error) => {
                status = status.max(CheckStatus::Warning);
                summary = "npm update target could not be inspected".to_string();
                details.push(format!("npm root -g failed: {error}"));
            }
        }
    }

    match fetch_latest_version(&install_context) {
        Ok(latest_version) => {
            details.push(format!("latest version: {latest_version}"));
            if is_newer(&latest_version, env!("CARGO_PKG_VERSION")) == Some(true) {
                details.push("latest version status: newer version is available".to_string());
            } else {
                details.push("latest version status: current version is not older".to_string());
            }
        }
        Err(err) => {
            status = status.max(CheckStatus::Warning);
            details.push(format!("latest version probe: {err}"));
        }
    }

    let mut check = DoctorCheck::new("updates.status", "updates", status, summary).details(details);
    if let Some(remediation) = remediation {
        check = check.remediation(remediation);
    }
    check
}

fn push_cached_version_details(details: &mut Vec<String>, version_file: &Path) {
    details.push(format!("version cache: {}", version_file.display()));
    match std::fs::read_to_string(version_file) {
        Ok(contents) => match serde_json::from_str::<VersionInfo>(&contents) {
            Ok(info) => {
                details.push(format!("cached latest version: {}", info.latest_version));
                if let Some(last_checked_at) = info.last_checked_at {
                    details.push(format!("last checked at: {last_checked_at}"));
                }
                if let Some(dismissed_version) = info.dismissed_version {
                    details.push(format!("dismissed version: {dismissed_version}"));
                }
            }
            Err(err) => details.push(format!("version cache parse: {err}")),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            details.push("version cache: missing".to_string());
        }
        Err(err) => details.push(format!("version cache read: {err}")),
    }
}

fn update_action_label(context: &InstallContext) -> String {
    match &context.method {
        InstallMethod::Npm => format!("npm install -g {CODEX_UPDATE_NPM_PACKAGE}"),
        InstallMethod::Bun => format!("bun install -g {CODEX_UPDATE_NPM_PACKAGE}"),
        InstallMethod::Brew => format!("brew upgrade --cask {CODEX_UPDATE_BREW_CASK}"),
        InstallMethod::Standalone { .. } => "standalone installer".to_string(),
        InstallMethod::Other => "manual or unknown".to_string(),
    }
}

fn fetch_latest_version(context: &InstallContext) -> Result<String, String> {
    match &context.method {
        InstallMethod::Brew => fetch_homebrew_cask_version(),
        InstallMethod::Npm
        | InstallMethod::Bun
        | InstallMethod::Standalone { .. }
        | InstallMethod::Other => fetch_latest_github_release_version(),
    }
}

fn fetch_latest_github_release_version() -> Result<String, String> {
    #[derive(Deserialize)]
    struct ReleaseInfo {
        tag_name: String,
    }

    let info = http_get_json::<ReleaseInfo>(&github_latest_release_url())?;
    extract_version_from_latest_tag(&info.tag_name)
        .ok_or_else(|| format!("failed to parse latest tag {}", info.tag_name))
}

fn github_latest_release_url() -> String {
    format!("https://api.github.com/repos/{CODEX_RELEASE_REPOSITORY}/releases/latest")
}

fn extract_version_from_latest_tag(latest_tag_name: &str) -> Option<String> {
    [CODEX_RELEASE_TAG_PREFIX, "v", "rust-v"]
        .into_iter()
        .find_map(|prefix| latest_tag_name.strip_prefix(prefix))
        .map(str::to_string)
}

fn fetch_homebrew_cask_version() -> Result<String, String> {
    #[derive(Deserialize)]
    struct HomebrewCaskInfo {
        version: String,
    }

    http_get_json::<HomebrewCaskInfo>(HOMEBREW_CASK_API_URL).map(|info| info.version)
}

fn http_get_json<T>(url: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let body = run_command("curl", ["-fsSL", "--max-time", "5", url])?;
    serde_json::from_str::<T>(&body).map_err(|err| err.to_string())
}

fn is_newer(latest: &str, current: &str) -> Option<bool> {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => Some(latest > current),
        (Some(_), None) | (None, Some(_)) | (None, None) => None,
    }
}

fn parse_version(value: &str) -> Option<Version> {
    Version::parse(value.trim()).ok()
}

#[derive(Deserialize)]
struct VersionInfo {
    latest_version: String,
    #[serde(default)]
    last_checked_at: Option<String>,
    #[serde(default)]
    dismissed_version: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_newer_compares_plain_semver() {
        assert_eq!(is_newer("1.2.4", "1.2.3"), Some(true));
        assert_eq!(is_newer("1.2.3", "1.2.4"), Some(false));
        assert_eq!(is_newer("1.2.3-beta.1", "1.2.2"), Some(true));
    }

    #[test]
    fn extracts_configured_release_tag_versions() {
        assert_eq!(
            extract_version_from_latest_tag("v0.134.0-sedna.2+upstream.74"),
            Some("0.134.0-sedna.2+upstream.74".to_string())
        );
        assert_eq!(
            extract_version_from_latest_tag("rust-v0.143.0"),
            Some("0.143.0".to_string())
        );
        assert_eq!(extract_version_from_latest_tag("release-0.143.0"), None);
    }

    #[test]
    fn update_action_labels_install_contexts() {
        assert_eq!(
            update_action_label(&InstallContext {
                method: InstallMethod::Npm,
                package_layout: None,
            }),
            "npm install -g @openai/codex".to_string()
        );
        assert_eq!(
            update_action_label(&InstallContext {
                method: InstallMethod::Other,
                package_layout: None,
            }),
            "manual or unknown".to_string()
        );
    }
}
