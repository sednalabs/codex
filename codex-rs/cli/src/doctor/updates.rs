//! Diagnoses whether Codex update paths target the running installation.
//!
//! Update diagnostics combine cache metadata with a bounded probe of the Sedna
//! release channel. Other build channels fail closed: doctor must not suggest
//! that an upstream package manager can update a Sedna binary.

use std::cmp::Ordering;
use std::path::Path;

use codex_core::config::Config;
use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
use codex_install_context::StandalonePlatform;
use codex_utils_version::RELEASE_VERSION;
use serde::Deserialize;

use super::CheckStatus;
use super::DoctorCheck;
use super::doctor_install_context;
use super::run_command;

const VERSION_FILE_NAME: &str = "version.json";
const SEDNA_RELEASE_REPOSITORY: &str = "sednalabs/codex";
const LOCALLY_CONSISTENT_SUMMARY: &str = "update configuration is locally consistent";
const NEWER_SEDNA_RELEASE_SUMMARY: &str = "newer Sedna release is available";
const INVALID_SEDNA_VERSION_SUMMARY: &str = "Sedna release versions could not be compared";
const SEDNA_RELEASE_PROBE_WARNING_SUMMARY: &str = "Sedna release probe failed";

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
    let mut summary = LOCALLY_CONSISTENT_SUMMARY.to_string();
    if is_sedna_release_channel() {
        match fetch_latest_sedna_release_version() {
            Ok(latest_version) => {
                details.push(format!("latest Sedna release: {latest_version}"));
                let comparison = is_newer_sedna_release(&latest_version, RELEASE_VERSION);
                let (comparison_status, comparison_summary) =
                    sedna_release_comparison_status(comparison);
                status = status.max(comparison_status);
                summary = comparison_summary.to_string();
                match comparison {
                    Some(true) => {
                        details
                            .push("latest version status: newer version is available".to_string());
                    }
                    Some(false) => {
                        details.push(
                            "latest version status: current version is not older".to_string(),
                        );
                    }
                    None => {
                        details.push(
                            "latest version status: running version is not a valid Sedna release"
                                .to_string(),
                        );
                    }
                }
            }
            Err(err) => {
                let (probe_status, probe_summary) = sedna_release_probe_failure_status();
                status = status.max(probe_status);
                summary = probe_summary.to_string();
                details.push(format!("latest version probe: {err}"));
            }
        }
    } else {
        details.push("latest version probe: unavailable for this build identity".to_string());
    }

    DoctorCheck::new("updates.status", "updates", status, summary).details(details)
}

fn sedna_release_comparison_status(is_newer: Option<bool>) -> (CheckStatus, &'static str) {
    match is_newer {
        Some(true) => (CheckStatus::Ok, NEWER_SEDNA_RELEASE_SUMMARY),
        Some(false) => (CheckStatus::Ok, LOCALLY_CONSISTENT_SUMMARY),
        None => (CheckStatus::Warning, INVALID_SEDNA_VERSION_SUMMARY),
    }
}

const fn sedna_release_probe_failure_status() -> (CheckStatus, &'static str) {
    (CheckStatus::Warning, SEDNA_RELEASE_PROBE_WARNING_SUMMARY)
}

fn push_cached_version_details(details: &mut Vec<String>, version_file: &Path) {
    details.push(format!("version cache: {}", version_file.display()));
    match std::fs::read_to_string(version_file) {
        Ok(contents) => match serde_json::from_str::<VersionInfo>(&contents) {
            Ok(info) => {
                push_cache_info_details(details, &info, is_sedna_release_channel());
            }
            Err(err) => details.push(format!("version cache parse: {err}")),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            details.push("version cache: missing".to_string());
        }
        Err(err) => details.push(format!("version cache read: {err}")),
    }
}

fn push_cache_info_details(
    details: &mut Vec<String>,
    info: &VersionInfo,
    has_current_sedna_identity: bool,
) {
    if !has_current_sedna_identity || !info.matches_sedna_release_identity() {
        details.push("version cache: ignored (untrusted release identity)".to_string());
        return;
    }

    details.push(format!("cached latest version: {}", info.latest_version));
    if let Some(last_checked_at) = &info.last_checked_at {
        details.push(format!("last checked at: {last_checked_at}"));
    }
    if let Some(dismissed_version) = &info.dismissed_version {
        details.push(format!("dismissed version: {dismissed_version}"));
    }
}

fn update_action_label(context: &InstallContext) -> &'static str {
    update_action_label_for_sedna_identity(context, is_sedna_release_channel(), RELEASE_VERSION)
}

fn update_action_label_for_sedna_identity(
    context: &InstallContext,
    has_sedna_identity: bool,
    release_version: &str,
) -> &'static str {
    update_action_label_for_sedna_identity_on_target(
        context,
        has_sedna_identity,
        release_version,
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

fn update_action_label_for_sedna_identity_on_target(
    context: &InstallContext,
    has_sedna_identity: bool,
    release_version: &str,
    target_os: &str,
    target_arch: &str,
) -> &'static str {
    if !has_sedna_identity {
        return "no automatic update action outside the Sedna release channel";
    }
    if !is_valid_sedna_release_version(release_version) {
        return "no automatic update action";
    }
    if !codex_utils_version::is_sedna_standalone_update_target_supported(target_os, target_arch) {
        return "no automatic update action";
    }
    match &context.method {
        InstallMethod::Standalone {
            platform: StandalonePlatform::Unix,
            ..
        } => "Sedna standalone installer",
        InstallMethod::Standalone {
            platform: StandalonePlatform::Windows,
            ..
        } => "no automatic update action",
        InstallMethod::Npm
        | InstallMethod::Bun
        | InstallMethod::Pnpm
        | InstallMethod::Brew
        | InstallMethod::Other => "no automatic update action",
    }
}

fn is_valid_sedna_release_version(release_version: &str) -> bool {
    parse_sedna_release_tag(&format!("v{release_version}")).is_some()
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

fn is_newer_sedna_release(latest: &str, current: &str) -> Option<bool> {
    let latest = SednaReleaseVersion::parse(latest)?;
    let current = SednaReleaseVersion::parse(current)?;
    Some(latest > current)
}

#[derive(Debug, Eq, PartialEq)]
struct SednaReleaseVersion {
    core: [String; 3],
    track_prerelease: Vec<String>,
    ordinal: String,
}

impl SednaReleaseVersion {
    fn parse(version: &str) -> Option<Self> {
        let version = parse_sedna_release_tag(&format!("v{version}"))?;
        let version_without_metadata = version.split_once('+').map_or(version.as_str(), |(v, _)| v);
        let (track, ordinal) = version_without_metadata.rsplit_once("-sedna.")?;
        let (core, track_prerelease) = match track.split_once('-') {
            Some((core, prerelease)) => (core, prerelease.split('.').map(str::to_owned).collect()),
            None => (track, Vec::new()),
        };
        let mut core_parts = core.split('.').map(str::to_owned);
        let core = [core_parts.next()?, core_parts.next()?, core_parts.next()?];
        if core_parts.next().is_some() {
            return None;
        }
        Some(Self {
            core,
            track_prerelease,
            ordinal: ordinal.to_string(),
        })
    }
}

impl Ord for SednaReleaseVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        for (left, right) in self.core.iter().zip(&other.core) {
            let ordering = compare_numeric_identifiers(left, right);
            if ordering != Ordering::Equal {
                return ordering;
            }
        }
        let prerelease_ordering = match (
            self.track_prerelease.is_empty(),
            other.track_prerelease.is_empty(),
        ) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => {
                compare_prerelease_identifiers(&self.track_prerelease, &other.track_prerelease)
            }
        };
        if prerelease_ordering != Ordering::Equal {
            return prerelease_ordering;
        }
        compare_numeric_identifiers(&self.ordinal, &other.ordinal)
    }
}

fn compare_prerelease_identifiers(left: &[String], right: &[String]) -> Ordering {
    for (left, right) in left.iter().zip(right) {
        let ordering = match (
            left.bytes().all(|byte| byte.is_ascii_digit()),
            right.bytes().all(|byte| byte.is_ascii_digit()),
        ) {
            (true, true) => compare_numeric_identifiers(left, right),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

impl PartialOrd for SednaReleaseVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_numeric_identifiers(left: &str, right: &str) -> Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

#[derive(Deserialize)]
struct VersionInfo {
    latest_version: String,
    #[serde(default)]
    last_checked_at: Option<String>,
    #[serde(default)]
    dismissed_version: Option<String>,
    #[serde(default)]
    release_repository: Option<String>,
    #[serde(default)]
    release_tag_prefix: Option<String>,
}

impl VersionInfo {
    fn matches_sedna_release_identity(&self) -> bool {
        self.release_repository.as_deref() == Some(SEDNA_RELEASE_REPOSITORY)
            && self.release_tag_prefix.as_deref() == Some("v")
    }
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
    fn doctor_action_label_offers_sedna_installer_only_for_unix_standalone() {
        let native_release_dir = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            std::env::temp_dir().join("native-release"),
        )
        .expect("temp dir path should be absolute");
        let unix = InstallContext {
            method: InstallMethod::Standalone {
                platform: StandalonePlatform::Unix,
                release_dir: native_release_dir.clone(),
                resources_dir: None,
            },
            package_layout: None,
        };
        let windows = InstallContext {
            method: InstallMethod::Standalone {
                platform: StandalonePlatform::Windows,
                release_dir: native_release_dir,
                resources_dir: None,
            },
            package_layout: None,
        };
        assert_eq!(
            update_action_label_for_sedna_identity_on_target(
                &unix,
                true,
                "1.2.3-sedna.4",
                "linux",
                "x86_64",
            ),
            "Sedna standalone installer"
        );
        assert_eq!(
            update_action_label_for_sedna_identity(&windows, true, "1.2.3-sedna.4"),
            "no automatic update action"
        );
        for release_version in ["1.2.3", "not-a-Sedna-release"] {
            assert_eq!(
                update_action_label_for_sedna_identity(&unix, true, release_version),
                "no automatic update action",
                "accepted {release_version}"
            );
        }
        for target in [("macos", "aarch64"), ("freebsd", "x86_64")] {
            assert_eq!(
                update_action_label_for_sedna_identity_on_target(
                    &unix,
                    true,
                    "1.2.3-sedna.4",
                    target.0,
                    target.1,
                ),
                "no automatic update action",
                "accepted unsupported target {}-{}",
                target.0,
                target.1
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

    #[test]
    fn compares_only_valid_sedna_release_versions() {
        assert_eq!(
            is_newer_sedna_release("1.2.3-sedna.2", "1.2.3-sedna.1"),
            Some(true)
        );
        assert_eq!(
            is_newer_sedna_release("1.2.3-sedna.1", "1.2.3-sedna.2"),
            Some(false)
        );
        assert_eq!(
            is_newer_sedna_release("1.2.3-alpha.2-sedna.1", "1.2.3-alpha.1-sedna.9"),
            Some(true)
        );
        assert_eq!(
            is_newer_sedna_release("1.2.3-alpha.10-sedna.1", "1.2.3-alpha.2-sedna.99"),
            Some(true)
        );
        assert_eq!(is_newer_sedna_release("1.2.3", "1.2.2-sedna.1"), None);
        assert_eq!(
            is_newer_sedna_release("1.2.3-sedna.2", "1.2.2+upstream.3"),
            None
        );
    }

    #[test]
    fn sedna_release_summary_reports_newer_and_warning_states() {
        assert_eq!(
            sedna_release_comparison_status(Some(true)),
            (CheckStatus::Ok, NEWER_SEDNA_RELEASE_SUMMARY)
        );
        assert_eq!(
            sedna_release_comparison_status(Some(false)),
            (CheckStatus::Ok, LOCALLY_CONSISTENT_SUMMARY)
        );
        assert_eq!(
            sedna_release_comparison_status(None),
            (CheckStatus::Warning, INVALID_SEDNA_VERSION_SUMMARY)
        );
        assert_eq!(
            sedna_release_probe_failure_status(),
            (CheckStatus::Warning, SEDNA_RELEASE_PROBE_WARNING_SUMMARY)
        );
    }

    #[test]
    fn matching_cache_identity_retains_cached_version_details() {
        let info = VersionInfo {
            latest_version: "1.2.3-sedna.2".to_string(),
            last_checked_at: Some("2026-08-20T00:00:00Z".to_string()),
            dismissed_version: Some("1.2.3-sedna.2".to_string()),
            release_repository: Some("sednalabs/codex".to_string()),
            release_tag_prefix: Some("v".to_string()),
        };
        let mut details = Vec::new();

        push_cache_info_details(&mut details, &info, true);

        assert_eq!(
            details,
            vec![
                "cached latest version: 1.2.3-sedna.2",
                "last checked at: 2026-08-20T00:00:00Z",
                "dismissed version: 1.2.3-sedna.2",
            ]
        );
    }

    #[test]
    fn source_less_cache_identity_is_ignored() {
        let info = VersionInfo {
            latest_version: "1.2.3-sedna.2".to_string(),
            last_checked_at: None,
            dismissed_version: Some("1.2.3-sedna.2".to_string()),
            release_repository: None,
            release_tag_prefix: None,
        };
        let mut details = Vec::new();

        push_cache_info_details(&mut details, &info, true);

        assert_eq!(
            details,
            vec!["version cache: ignored (untrusted release identity)"]
        );
    }

    #[test]
    fn mismatched_cache_identity_is_ignored() {
        let info = VersionInfo {
            latest_version: "999.0.0".to_string(),
            last_checked_at: None,
            dismissed_version: Some("999.0.0".to_string()),
            release_repository: Some("openai/codex".to_string()),
            release_tag_prefix: Some("rust-v".to_string()),
        };
        let mut details = Vec::new();

        push_cache_info_details(&mut details, &info, true);

        assert_eq!(
            details,
            vec!["version cache: ignored (untrusted release identity)"]
        );
    }
}
