//! Diagnoses whether Codex update paths target the running installation.
//!
//! Update diagnostics combine cache metadata with a bounded probe of the Sedna
//! release channel. Other build channels fail closed: doctor must not suggest
//! that an upstream package manager can update a Sedna binary.

use std::path::Path;

use codex_core::config::Config;
use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use serde::Deserialize;

use super::CheckStatus;
use super::DoctorCheck;
use super::doctor_install_context;
use super::run_command;

const VERSION_FILE_NAME: &str = "version.json";
const SEDNA_RELEASE_REPOSITORY: &str = "sednalabs/codex";

/// Builds the update-health row for the current installation.
///
/// Network failures while fetching latest-version metadata degrade the row to a
/// warning instead of failing doctor outright; update freshness is useful
/// support context but should not mask more direct install/config failures.
pub(super) fn updates_check(config: &Config) -> DoctorCheck {
    let install_context = doctor_install_context(std::env::current_exe().ok().as_deref());
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
    let summary = "update configuration is locally consistent".to_string();
    match fetch_latest_sedna_release_version() {
        Ok(latest_version) => {
            details.push(format!("latest Sedna release: {latest_version}"));
            details.push("latest release status: Sedna release source verified".to_string());
        }
        Err(err) => {
            status = status.max(CheckStatus::Warning);
            details.push(format!("latest version probe: {err}"));
        }
    }

    DoctorCheck::new("updates.status", "updates", status, summary).details(details)
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

fn update_action_label(context: &InstallContext) -> &'static str {
    if !is_sedna_release_channel() {
        return "no automatic update action outside the Sedna release channel";
    }
    match &context.method {
        InstallMethod::Standalone { .. } => "Sedna standalone installer",
        InstallMethod::Npm
        | InstallMethod::Bun
        | InstallMethod::Pnpm
        | InstallMethod::Brew
        | InstallMethod::Other => "no automatic update action",
    }
}

fn is_sedna_release_channel() -> bool {
    is_sedna_release_identity(
        option_env!("CODEX_RELEASE_REPOSITORY"),
        option_env!("CODEX_RELEASE_TAG_PREFIX"),
    )
}

const fn is_sedna_release_identity(repository: Option<&str>, tag_prefix: Option<&str>) -> bool {
    matches!(repository, Some(SEDNA_RELEASE_REPOSITORY)) && matches!(tag_prefix, Some("v"))
}

fn fetch_latest_sedna_release_version() -> Result<String, String> {
    if !is_sedna_release_channel() {
        return Err(
            "latest release probe is unavailable outside the Sedna release channel".to_string(),
        );
    }
    let url = format!("https://api.github.com/repos/{SEDNA_RELEASE_REPOSITORY}/releases/latest");
    let info = http_get_json::<ReleaseInfo>(&url)?;
    parse_sedna_release_tag(&info.tag_name)
        .ok_or_else(|| format!("failed to parse Sedna release tag {}", info.tag_name))
}

#[derive(Deserialize)]
struct ReleaseInfo {
    tag_name: String,
}

fn http_get_json<T>(url: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    let body = run_command("curl", ["-fsSL", "--max-time", "5", url])?;
    serde_json::from_str::<T>(&body).map_err(|err| err.to_string())
}

fn parse_sedna_release_tag(tag: &str) -> Option<String> {
    let version = tag.strip_prefix('v')?;
    let (version, metadata) = match version.split_once('+') {
        Some((version, metadata)) => (version, Some(metadata)),
        None => (version, None),
    };
    if metadata.is_some_and(|metadata| {
        metadata.strip_prefix("upstream.").is_none_or(|distance| {
            distance.is_empty() || !distance.bytes().all(|b| b.is_ascii_digit())
        })
    }) {
        return None;
    }
    let (track, ordinal) = version.rsplit_once("-sedna.")?;
    if ordinal.is_empty() || !ordinal.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (core, prerelease) = match track.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (track, None),
    };
    let mut core_parts = core.split('.');
    let valid_core = (0..3).all(|_| {
        core_parts
            .next()
            .is_some_and(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
    }) && core_parts.next().is_none();
    let valid_prerelease = prerelease.is_none_or(|prerelease| {
        !prerelease.is_empty()
            && prerelease.split('.').all(|identifier| {
                !identifier.is_empty() && identifier.bytes().all(|b| b.is_ascii_alphanumeric())
            })
    });
    (valid_core && valid_prerelease).then(|| version.to_string())
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
    fn parses_only_sedna_release_tags() {
        assert_eq!(
            parse_sedna_release_tag("v1.2.3-alpha.4-sedna.2+upstream.17"),
            Some("1.2.3-alpha.4-sedna.2+upstream.17".to_string())
        );
        for tag in [
            "rust-v1.2.3",
            "v1.2.3",
            "v1.2.3-sedna.x",
            "v1.2.3-sedna.1+build.2",
            "v1.2.3-sedna.1+upstream.x",
            "v1.2.3-rc-1-sedna.1",
        ] {
            assert_eq!(parse_sedna_release_tag(tag), None, "accepted {tag}");
        }
    }

    #[test]
    fn update_action_labels_never_suggest_upstream_package_managers() {
        for method in [
            InstallMethod::Npm,
            InstallMethod::Pnpm,
            InstallMethod::Other,
        ] {
            assert!(
                !update_action_label(&InstallContext {
                    method,
                    package_layout: None,
                })
                .contains("openai")
            );
        }
    }

    #[test]
    fn sedna_update_identity_requires_both_explicit_build_values() {
        assert!(is_sedna_release_identity(
            Some("sednalabs/codex"),
            Some("v")
        ));
        for identity in [
            (None, None),
            (Some("sednalabs/codex"), None),
            (None, Some("v")),
            (Some("openai/codex"), Some("v")),
            (Some("sednalabs/codex"), Some("rust-v")),
        ] {
            assert!(!is_sedna_release_identity(identity.0, identity.1));
        }
    }
}
