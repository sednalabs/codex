//! Diagnoses whether Codex update paths target the running installation.
//!
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
//! Update diagnostics combine cache metadata with a bounded probe of the Sedna
//! release channel. Other build channels fail closed: doctor must not suggest
//! that an upstream package manager can update a Sedna binary.
=======
//! Update diagnostics combine cached version metadata, install-channel hints,
//! and bounded latest-version HTTP probes. It never executes package managers or
//! other helpers selected by PATH; npm update targets are not verified.
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;
use std::time::Duration;

use codex_core::config::Config;
use codex_http_client::ClientRouteClass;
use codex_http_client::RouteAwareClientPool;
use codex_install_context::InstallContext;
use codex_install_context::InstallMethod;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
use codex_install_context::StandalonePlatform;
use codex_utils_version::RELEASE_VERSION;
use codex_utils_version::SednaReleaseChannel;
use codex_utils_version::is_newer_sedna_release;
use codex_utils_version::is_sedna_automatic_update_eligible_for_channel;
use codex_utils_version::is_sedna_release_identity;
use codex_utils_version::parse_sedna_release_tag;
=======
use http::Method;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
use serde::Deserialize;
#[cfg(target_os = "macos")]
use url::Url;

use super::CheckStatus;
use super::DoctorCheck;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
use super::doctor_install_context;
use super::run_command;

const VERSION_FILE_NAME: &str = "version.json";
const LOCALLY_CONSISTENT_SUMMARY: &str = "update configuration is locally consistent";
const NEWER_SEDNA_RELEASE_SUMMARY: &str = "newer Sedna release is available";
const INVALID_SEDNA_VERSION_SUMMARY: &str = "Sedna release versions could not be compared";
const SEDNA_RELEASE_PROBE_WARNING_SUMMARY: &str = "Sedna release probe failed";
=======
#[cfg(any(target_os = "macos", target_os = "windows"))]
use super::DoctorIssue;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use super::desktop::platform::InstalledApp;
use super::doctor_install_context;
use super::doctor_managed_by_npm;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use super::network;

const MAX_VERSION_RESPONSE_BYTES: usize = 1024 * 1024;

const VERSION_FILE_NAME: &str = "version.json";
const GITHUB_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";
const HOMEBREW_CASK_API_URL: &str = "https://formulae.brew.sh/api/cask/codex.json";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const DESKTOP_UPDATE_URL: &str = "https://persistent.oaistatic.com/codex-app-prod/appcast-x64.xml";
#[cfg(all(target_os = "macos", not(target_arch = "x86_64")))]
const DESKTOP_UPDATE_URL: &str = "https://persistent.oaistatic.com/codex-app-prod/appcast.xml";
#[cfg(target_os = "macos")]
const BACKEND_DESKTOP_UPDATE_URL: &str = "https://chatgpt.com/backend-api/wham/app/appcast";
#[cfg(target_os = "windows")]
const DESKTOP_UPDATE_URL: &str =
    "https://persistent.oaistatic.com/codex-app-prod/windows-store-update.json";
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360

/// Builds the update-health row for the current installation.
///
/// Network failures while fetching latest-version metadata degrade the row to a
/// warning instead of failing doctor outright; update freshness is useful
/// support context but should not mask more direct install/config failures.
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
pub(super) fn updates_check(config: &Config) -> DoctorCheck {
    let install_context = doctor_install_context(std::env::current_exe().ok().as_deref());
=======
pub(super) async fn updates_check(config: &Config) -> DoctorCheck {
    let current_exe = std::env::current_exe().ok();
    let install_context = doctor_install_context(current_exe.as_deref());
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    let mut details = vec![
        format!(
            "check for update on startup: {}",
            config.check_for_update_on_startup
        ),
        format!(
            "selected release channel: {}",
            serde_json::to_string(&config.sedna_release_channel)
                .unwrap_or_else(|_| "\"stable\"".to_string())
                .trim_matches('\"')
        ),
        format!(
            "update action: {}",
            update_action_label(&install_context, config.sedna_release_channel)
        ),
    ];
    let version_file = config.codex_home.join(VERSION_FILE_NAME);
    push_cached_version_details(
        &mut details,
        &version_file,
        &install_context,
        config.sedna_release_channel,
    );

    let mut status = CheckStatus::Ok;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    let mut summary = LOCALLY_CONSISTENT_SUMMARY.to_string();
    if is_sedna_automatic_update_probe_available(&install_context, config.sedna_release_channel) {
        match fetch_latest_sedna_release_version(&install_context, config.sedna_release_channel) {
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
        details
            .push("latest version probe: unavailable for this automatic-update policy".to_string());
=======
    let summary = "update configuration is locally consistent".to_string();

    if doctor_managed_by_npm(current_exe.as_deref()) {
        details
            .push("npm update target: not inspected (PATH helpers are not executed)".to_string());
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    }
    let client = RouteAwareClientPool::new_without_request_logging(
        config.http_client_factory(),
        ClientRouteClass::Other,
    );

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    DoctorCheck::new("updates.status", "updates", status, summary).details(details)
=======
    match fetch_latest_version(&client, &install_context).await {
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

    DoctorCheck::new("updates.status", "updates", status, summary).details(details)
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
pub(super) async fn append_desktop_update(
    checks: &mut [DoctorCheck],
    config: Option<&Config>,
    application: &InstalledApp,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    #[cfg(target_os = "macos")]
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from)
        && let Some(build) = latest_macos_staged_build(
            &home
                .join("Library/Caches")
                .join(application.identity)
                .join("org.sparkle-project.Sparkle/Installation"),
            application.build,
        )
        .await
        && let Some(update) = checks.iter_mut().find(|check| check.id == "updates.status")
    {
        update.details.extend([
            "desktop update status: ready to install".to_string(),
            format!("desktop latest build: {build}"),
            format!("desktop application: {}", application.identity),
        ]);
    }

    let Some(config) = config else {
        return;
    };
    let Some(reachability_index) = checks
        .iter()
        .position(|check| check.id == "network.provider_reachability")
    else {
        return;
    };
    #[cfg(target_os = "macos")]
    let desktop_update_url = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| {
            macos_desktop_update_url(&home, application, &os_info::get().version().to_string())
        })
        .unwrap_or_else(|| DESKTOP_UPDATE_URL.to_string());
    #[cfg(target_os = "windows")]
    let desktop_update_url = DESKTOP_UPDATE_URL;
    #[cfg(target_os = "macos")]
    let desktop_update_url = desktop_update_url.as_str();
    let desktop_update_display_url = desktop_update_url
        .split_once('?')
        .map_or(desktop_update_url, |(endpoint, _)| endpoint);
    let client = RouteAwareClientPool::new_without_request_logging(
        config.http_client_factory(),
        ClientRouteClass::Other,
    );
    let outcome = match client
        .request(Method::GET, desktop_update_url)
        .timeout(deadline.saturating_duration_since(tokio::time::Instant::now()))
        .send()
        .await
    {
        Ok(response) => {
            let status = response.status().as_u16();
            #[cfg(target_os = "windows")]
            if status == 404 && response.url().scheme() == "https" {
                checks[reachability_index].details.push(format!(
                    "desktop assets CDN: {desktop_update_display_url} reachable (HTTP 404; no update available)"
                ));
                return;
            }
            if cfg!(target_os = "windows") && response.url().scheme() != "https" {
                Err("update manifest redirected to a non-HTTPS URL".to_string())
            } else if status == 407 {
                Err("proxy authentication required (HTTP 407)".to_string())
            } else if !(200..=299).contains(&status) {
                Err(format!("HTTP {status}"))
            } else {
                checks[reachability_index].details.push(format!(
                    "desktop assets CDN: {desktop_update_display_url} reachable (HTTP {status})"
                ));
                #[cfg(target_os = "windows")]
                if let Some(update) = checks.iter_mut().find(|check| check.id == "updates.status") {
                    match response.bytes().await {
                        Ok(body) => match windows_store_update(&body, &application.version) {
                            Ok(Some(build)) => update.details.extend([
                                "desktop update status: available".to_string(),
                                format!("desktop latest build: {build}"),
                                format!("desktop application: {}", application.identity),
                            ]),
                            Ok(None) => {}
                            Err(error) => {
                                update.status = update.status.max(CheckStatus::Warning);
                                update
                                    .details
                                    .push(format!("desktop update manifest: {error}"));
                            }
                        },
                        Err(_) => {
                            update.status = update.status.max(CheckStatus::Warning);
                            update
                                .details
                                .push("desktop update manifest: response could not be read".into());
                        }
                    }
                }
                Ok(())
            }
        }
        Err(error) => Err(network::request_error(error)),
    };

    if let Err(error) = outcome {
        let reachability = &mut checks[reachability_index];
        reachability.details.push(format!(
            "desktop assets CDN: {desktop_update_display_url} {error} (optional)"
        ));
        if reachability.status == CheckStatus::Ok {
            reachability.status = CheckStatus::Warning;
            reachability.summary = "desktop update and runtime CDN is unreachable".to_string();
        }
        reachability.issues.push(
            DoctorIssue::new(
                CheckStatus::Warning,
                "desktop update and runtime CDN is unreachable",
            )
            .measured(format!("{desktop_update_display_url} {error}"))
            .expected("desktop update and runtime CDN reachable over HTTPS")
            .remedy(
                if desktop_update_display_url.starts_with("https://chatgpt.com/") {
                    "check proxy, firewall, DNS, and certificate access to chatgpt.com"
                } else {
                    "check proxy, firewall, DNS, and certificate access to persistent.oaistatic.com"
                },
            )
            .field("desktop assets CDN"),
        );
    }
}

#[cfg(target_os = "macos")]
fn macos_desktop_update_url(home: &Path, application: &InstalledApp, os_version: &str) -> String {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ProductionAppcastState {
        #[serde(default)]
        backend_appcast_enabled: bool,
        installation_id: Option<String>,
    }

    let state_path = home
        .join("Library/Application Support")
        .join(application.identity)
        .join("production-appcast-bootstrap.json");
    let Some(state) = std::fs::read(state_path)
        .ok()
        .and_then(|contents| serde_json::from_slice::<ProductionAppcastState>(&contents).ok())
    else {
        return DESKTOP_UPDATE_URL.to_string();
    };
    let Some(installation_id) = state
        .backend_appcast_enabled
        .then_some(state.installation_id)
        .flatten()
    else {
        return DESKTOP_UPDATE_URL.to_string();
    };

    let Ok(mut url) = Url::parse(BACKEND_DESKTOP_UPDATE_URL) else {
        return DESKTOP_UPDATE_URL.to_string();
    };
    url.query_pairs_mut().extend_pairs([
        ("installation_id", installation_id.as_str()),
        (
            "arch",
            if cfg!(target_arch = "x86_64") {
                "x64"
            } else {
                "arm64"
            },
        ),
        ("app_version", application.version.as_str()),
        ("beta", "false"),
        ("os-version", os_version),
        ("plan_type", "unknown"),
    ]);
    url.to_string()
}

#[cfg(any(target_os = "windows", test))]
fn windows_store_update(
    manifest: &[u8],
    installed_version: &str,
) -> Result<Option<String>, &'static str> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct StoreManifest {
        schema_version: u64,
        build_version: String,
        store_product_id: String,
        package_identity: String,
    }

    let manifest: StoreManifest =
        serde_json::from_slice(manifest).map_err(|_| "invalid Windows Store update manifest")?;
    if manifest.schema_version == 0
        || manifest.store_product_id != "9PLM9XGG6VKS"
        || manifest.package_identity != "OpenAI.Codex"
    {
        return Err("Windows Store update manifest does not target the production application");
    }
    let version = |value: &str| -> Option<[u64; 4]> {
        value
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?
            .try_into()
            .ok()
    };
    let latest = version(&manifest.build_version)
        .ok_or("Windows Store update manifest contains an invalid build version")?;
    let installed =
        version(installed_version).ok_or("installed Windows application has an invalid version")?;
    Ok((latest > installed).then_some(manifest.build_version))
}

#[cfg(target_os = "macos")]
async fn latest_macos_staged_build(root: &Path, installed_build: u64) -> Option<u64> {
    const MAX_STAGED_BUNDLES: usize = 64;

    if !std::fs::symlink_metadata(root).ok()?.is_dir() {
        return None;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let mut inspected = 0;
    let mut latest = None;
    for entry in std::fs::read_dir(root).ok()? {
        if inspected == MAX_STAGED_BUNDLES || tokio::time::Instant::now() >= deadline {
            break;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let extracted = entry.path().join("extracted");
        if !std::fs::symlink_metadata(&extracted).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        let bundle = extracted.join("ChatGPT.app");
        if !std::fs::symlink_metadata(&bundle).is_ok_and(|metadata| metadata.is_dir()) {
            continue;
        }
        inspected += 1;
        let Ok(result) = tokio::time::timeout_at(
            deadline,
            super::desktop::platform::inspect_macos_bundle(&bundle),
        )
        .await
        else {
            break;
        };
        if let Ok(Some(application)) = result
            && application.build > installed_build
        {
            latest = Some(latest.map_or(application.build, |latest: u64| {
                latest.max(application.build)
            }));
        }
    }
    latest
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
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

fn push_cached_version_details(
    details: &mut Vec<String>,
    version_file: &Path,
    install_context: &InstallContext,
    release_channel: SednaReleaseChannel,
) {
    details.push(format!("version cache: {}", version_file.display()));
    match std::fs::read_to_string(version_file) {
        Ok(contents) => match serde_json::from_str::<VersionInfo>(&contents) {
            Ok(info) => {
                push_cache_info_details(
                    details,
                    &info,
                    is_sedna_automatic_update_probe_available(install_context, release_channel),
                    release_channel,
                );
            }
            Err(err) => details.push(format!("version cache parse: {err}")),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            details.push("version cache: missing".to_string());
        }
        Err(err) => details.push(format!("version cache read: {err}")),
    }
}

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
fn push_cache_info_details(
    details: &mut Vec<String>,
    info: &VersionInfo,
    automatic_update_probe_available: bool,
    release_channel: SednaReleaseChannel,
) {
    if !automatic_update_probe_available {
        return;
    }
    if !info.matches_sedna_release_identity_and_channel(release_channel) {
        details.push("version cache: ignored (untrusted release identity or channel)".to_string());
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

fn update_action_label(
    context: &InstallContext,
    release_channel: SednaReleaseChannel,
) -> &'static str {
    update_action_label_for_sedna_identity(
        context,
        is_sedna_release_identity(
            option_env!("CODEX_RELEASE_REPOSITORY"),
            option_env!("CODEX_RELEASE_TAG_PREFIX"),
        ),
        RELEASE_VERSION,
        release_channel,
    )
}

fn update_action_label_for_sedna_identity(
    context: &InstallContext,
    has_sedna_identity: bool,
    release_version: &str,
    release_channel: SednaReleaseChannel,
) -> &'static str {
    update_action_label_for_sedna_identity_on_target(
        context,
        has_sedna_identity,
        release_version,
        std::env::consts::OS,
        std::env::consts::ARCH,
        release_channel,
    )
}

fn update_action_label_for_sedna_identity_on_target(
    context: &InstallContext,
    has_sedna_identity: bool,
    release_version: &str,
    target_os: &str,
    target_arch: &str,
    release_channel: SednaReleaseChannel,
) -> &'static str {
    if !has_sedna_identity {
        return "no automatic update action outside the Sedna release channel";
    }
    if !is_sedna_automatic_update_eligible_for_channel(
        release_version,
        target_os,
        target_arch,
        release_channel,
    ) {
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
=======
fn update_action_label(context: &InstallContext) -> &'static str {
    match &context.method {
        InstallMethod::Npm => "npm install -g @openai/codex",
        InstallMethod::Bun => "bun install -g @openai/codex",
        InstallMethod::VitePlus => "vp install -g @openai/codex",
        InstallMethod::Pnpm => "pnpm add -g @openai/codex",
        InstallMethod::Brew => "brew upgrade --cask codex",
        InstallMethod::Standalone { .. } => "standalone installer",
        InstallMethod::Other => "manual or unknown",
    }
}

async fn fetch_latest_version(
    client: &RouteAwareClientPool,
    context: &InstallContext,
) -> Result<String, String> {
    match &context.method {
        InstallMethod::Brew => fetch_homebrew_cask_version(client).await,
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
        InstallMethod::Npm
        | InstallMethod::Bun
        | InstallMethod::VitePlus
        | InstallMethod::Pnpm
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
        | InstallMethod::Brew
        | InstallMethod::Other => "no automatic update action",
    }
}

fn is_sedna_automatic_update_probe_available(
    context: &InstallContext,
    release_channel: SednaReleaseChannel,
) -> bool {
    is_sedna_automatic_update_probe_available_on_target(
        context,
        is_sedna_release_identity(
            option_env!("CODEX_RELEASE_REPOSITORY"),
            option_env!("CODEX_RELEASE_TAG_PREFIX"),
        ),
        RELEASE_VERSION,
        std::env::consts::OS,
        std::env::consts::ARCH,
        release_channel,
    )
}

fn is_sedna_automatic_update_probe_available_on_target(
    context: &InstallContext,
    has_sedna_identity: bool,
    release_version: &str,
    target_os: &str,
    target_arch: &str,
    release_channel: SednaReleaseChannel,
) -> bool {
    has_sedna_identity
        && is_sedna_automatic_update_eligible_for_channel(
            release_version,
            target_os,
            target_arch,
            release_channel,
        )
        && matches!(
            &context.method,
            InstallMethod::Standalone {
                platform: StandalonePlatform::Unix,
                ..
            }
        )
}

fn fetch_latest_sedna_release_version(
    context: &InstallContext,
    release_channel: SednaReleaseChannel,
) -> Result<String, String> {
    if !is_sedna_automatic_update_probe_available(context, release_channel) {
        return Err(
            "latest release probe is unavailable outside the Sedna standalone automatic-update policy"
                .to_string(),
        );
    }
    let url = format!(
        "https://api.github.com/repos/{}/releases?per_page=100",
        codex_utils_version::SEDNA_RELEASE_REPOSITORY
    );
    let releases = http_get_json::<Vec<ReleaseInfo>>(&url)?;
    select_latest_sedna_release(releases, release_channel)
}

#[derive(Deserialize)]
struct ReleaseInfo {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct ReleaseMetadata {
    release_tag: String,
    release_version: String,
    repository: String,
    target: String,
    #[serde(default)]
    release_channel: Option<SednaReleaseChannel>,
}

fn select_latest_sedna_release(
    releases: Vec<ReleaseInfo>,
    release_channel: SednaReleaseChannel,
) -> Result<String, String> {
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        _ => return Err("unsupported Sedna installer target".to_string()),
    };
    let metadata_name = if target == "x86_64-unknown-linux-gnu" {
        "RELEASE-METADATA.json".to_string()
    } else {
        format!("RELEASE-METADATA-{target}.json")
    };
    let mut candidates = releases
        .into_iter()
        .filter(|release| {
            !release.draft && release_channel.allows_api_prerelease(release.prerelease)
        })
        .filter_map(|release| {
            parse_sedna_release_tag(&release.tag_name).map(|version| (release, version))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|(_, left), (_, right)| {
        is_newer_sedna_release(left, right)
            .map(|newer| {
                if newer {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            })
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for (release, version) in candidates.into_iter().rev() {
        let Some(asset) = release
            .assets
            .iter()
            .find(|asset| asset.name == metadata_name)
        else {
            continue;
        };
        let metadata = http_get_json::<ReleaseMetadata>(&asset.browser_download_url)?;
        let api_channel = if release.prerelease {
            SednaReleaseChannel::Prerelease
        } else {
            SednaReleaseChannel::Stable
        };
        if release_metadata_matches(&metadata, &release.tag_name, &version, target, api_channel) {
            return Ok(version);
        }
    }
    Err("no valid published Sedna release matches the selected channel".to_string())
}

fn release_metadata_matches(
    metadata: &ReleaseMetadata,
    release_tag: &str,
    release_version: &str,
    target: &str,
    api_channel: SednaReleaseChannel,
) -> bool {
    metadata.release_tag == release_tag
        && metadata.release_version == release_version
        && metadata.repository == codex_utils_version::SEDNA_RELEASE_REPOSITORY
        && metadata.target == target
        // The GitHub API is the release-channel authority. Metadata may omit
        // the field for legacy releases, but it cannot contradict the API.
        && metadata.release_channel.is_none_or(|channel| channel == api_channel)
=======
        | InstallMethod::Standalone { .. }
        | InstallMethod::Other => fetch_latest_github_release_version(client).await,
    }
}

async fn fetch_latest_github_release_version(
    client: &RouteAwareClientPool,
) -> Result<String, String> {
    #[derive(Deserialize)]
    struct ReleaseInfo {
        tag_name: String,
    }

    let info = http_get_json::<ReleaseInfo>(client, GITHUB_LATEST_RELEASE_URL).await?;
    info.tag_name
        .strip_prefix("rust-v")
        .map(str::to_string)
        .ok_or_else(|| format!("failed to parse latest tag {}", info.tag_name))
}

async fn fetch_homebrew_cask_version(client: &RouteAwareClientPool) -> Result<String, String> {
    #[derive(Deserialize)]
    struct HomebrewCaskInfo {
        version: String,
    }

    http_get_json::<HomebrewCaskInfo>(client, HOMEBREW_CASK_API_URL)
        .await
        .map(|info| info.version)
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
}

async fn http_get_json<T>(client: &RouteAwareClientPool, url: &str) -> Result<T, String>
where
    T: for<'de> Deserialize<'de>,
{
    tokio::time::timeout(Duration::from_secs(/*secs*/ 5), async {
        let mut response = client
            .request(Method::GET, url)
            .header(
                http::header::USER_AGENT,
                concat!("codex-doctor/", env!("CARGO_PKG_VERSION")),
            )
            .send()
            .await
            .map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|err| err.to_string())? {
            if chunk.len() > MAX_VERSION_RESPONSE_BYTES.saturating_sub(body.len()) {
                return Err("version response exceeds size limit".to_string());
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|err| err.to_string())
    })
    .await
    .map_err(|_| "version request timed out".to_string())?
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
    #[serde(default)]
    release_channel: Option<SednaReleaseChannel>,
}

impl VersionInfo {
    fn matches_sedna_release_identity_and_channel(
        &self,
        release_channel: SednaReleaseChannel,
    ) -> bool {
        is_sedna_release_identity(
            self.release_repository.as_deref(),
            self.release_tag_prefix.as_deref(),
        ) && self.release_channel == Some(release_channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn version_http_probe_decodes_json_and_rejects_invalid_responses() {
        use codex_http_client::HttpClientFactory;
        use codex_http_client::OutboundProxyPolicy;
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::method;
        use wiremock::matchers::path;

        let server = MockServer::start().await;
        let client = RouteAwareClientPool::new_without_request_logging(
            HttpClientFactory::new(OutboundProxyPolicy::ReqwestDefault),
            ClientRouteClass::Other,
        );
        for (endpoint, response) in [
            (
                "valid",
                ResponseTemplate::new(/*s*/ 200)
                    .set_body_json(serde_json::json!({"version": "1.2.3"})),
            ),
            (
                "invalid",
                ResponseTemplate::new(/*s*/ 200).set_body_string("not JSON"),
            ),
            ("unavailable", ResponseTemplate::new(/*s*/ 503)),
            ("proxy_auth_required", ResponseTemplate::new(/*s*/ 407)),
            (
                "redirect",
                ResponseTemplate::new(/*s*/ 302)
                    .insert_header("Location", format!("{}/valid", server.uri())),
            ),
            (
                "timeout",
                ResponseTemplate::new(/*s*/ 200).set_delay(Duration::from_secs(/*secs*/ 6)),
            ),
            (
                "oversized",
                ResponseTemplate::new(/*s*/ 200)
                    .set_body_bytes(vec![b' '; MAX_VERSION_RESPONSE_BYTES + 1]),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/{endpoint}")))
                .respond_with(response)
                .mount(&server)
                .await;
            let result = http_get_json::<serde_json::Value>(
                &client,
                &format!("{}/{endpoint}", server.uri()),
            )
            .await;
            if matches!(endpoint, "valid" | "redirect") {
                assert_eq!(result, Ok(serde_json::json!({"version": "1.2.3"})));
            } else if endpoint == "timeout" {
                assert_eq!(result, Err("version request timed out".to_string()));
            } else if endpoint == "proxy_auth_required" {
                assert_eq!(
                    result,
                    Err("HTTP 407 Proxy Authentication Required".to_string())
                );
            } else {
                assert!(result.is_err(), "{endpoint} must not be accepted");
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_update_probe_uses_the_persisted_production_appcast_feed() {
        let home = tempfile::tempdir().expect("temporary home should be created");
        let application = InstalledApp {
            identity: "com.openai.codex",
            version: "26.623.10000".to_string(),
            bundle: PathBuf::new(),
            build: 6139,
        };
        assert_eq!(
            macos_desktop_update_url(home.path(), &application, "26.6.0"),
            DESKTOP_UPDATE_URL
        );

        let state_directory = home
            .path()
            .join("Library/Application Support/com.openai.codex");
        std::fs::create_dir_all(&state_directory)
            .expect("production appcast state directory should be created");
        std::fs::write(
            state_directory.join("production-appcast-bootstrap.json"),
            r#"{"backendAppcastEnabled":true,"installationId":"028e90f8-5f2a-47db-a05c-6a48f548d728"}"#,
        )
        .expect("production appcast state should be created");

        let arch = if cfg!(target_arch = "x86_64") {
            "x64"
        } else {
            "arm64"
        };
        assert_eq!(
            macos_desktop_update_url(home.path(), &application, "26.6.0"),
            format!(
                "{BACKEND_DESKTOP_UPDATE_URL}?installation_id=028e90f8-5f2a-47db-a05c-6a48f548d728&arch={arch}&app_version=26.623.10000&beta=false&os-version=26.6.0&plan_type=unknown"
            )
        );
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_staged_updates_require_a_newer_matching_extracted_bundle() {
        let root = tempfile::tempdir().expect("temporary Sparkle cache should be created");
        for index in 0..320 {
            std::fs::create_dir(root.path().join(format!("unrelated-{index}")))
                .expect("unrelated Sparkle cache directory should be created");
        }
        for (name, identity, build) in [
            ("newest", "com.openai.codex", "6268"),
            ("newer", "com.openai.codex", "6168"),
            ("older", "com.openai.codex", "6138"),
            ("different", "com.example.other", "9999"),
            ("invalid", "com.openai.codex", "invalid"),
        ] {
            let bundle = root.path().join(name).join("extracted/ChatGPT.app");
            write_macos_bundle(&bundle, identity, build);
        }
        let outside = tempfile::tempdir().expect("external fixture should be created");
        let linked = outside.path().join("ChatGPT.app");
        write_macos_bundle(&linked, "com.openai.codex", "9999");
        std::os::unix::fs::symlink(&linked, root.path().join("ChatGPT.app"))
            .expect("symlinked staged app fixture should be created");

        assert_eq!(
            latest_macos_staged_build(root.path(), /*installed_build*/ 6139).await,
            Some(6268)
        );
        assert_eq!(
            latest_macos_staged_build(root.path(), /*installed_build*/ 6268).await,
            None
        );
    }

    #[test]
    fn windows_store_updates_compare_all_four_production_build_components() {
        let mut manifest = serde_json::json!({
            "schemaVersion": 1,
            "buildVersion": "26.803.5235.1",
            "storeProductId": "9PLM9XGG6VKS",
            "packageIdentity": "OpenAI.Codex",
        });
        assert_eq!(
            windows_store_update(&serde_json::to_vec(&manifest).unwrap(), "26.803.5235.0"),
            Ok(Some("26.803.5235.1".to_string()))
        );
        assert_eq!(
            windows_store_update(&serde_json::to_vec(&manifest).unwrap(), "26.803.5235.1"),
            Ok(None)
        );
        manifest["storeProductId"] = "other".into();
        assert!(
            windows_store_update(&serde_json::to_vec(&manifest).unwrap(), "26.803.5235.0").is_err()
        );
    }

    #[cfg(target_os = "macos")]
    fn write_macos_bundle(path: &Path, identity: &str, build: &str) {
        let contents = path.join("Contents");
        std::fs::create_dir_all(&contents).expect("staged app fixture should be created");
        std::fs::write(
            contents.join("Info.plist"),
            format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
                 <key>CFBundleIdentifier</key><string>{identity}</string>\
                 <key>CFBundleVersion</key><string>{build}</string>\
                 </dict></plist>"
            ),
        )
        .expect("staged app metadata should be created");
    }

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
                !update_action_label(
                    &InstallContext {
                        method,
                        package_layout: None,
                    },
                    SednaReleaseChannel::Stable
                )
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
                /*has_sedna_identity*/ true,
                "1.2.3-sedna.4",
                "linux",
                "x86_64",
                SednaReleaseChannel::Stable,
            ),
            "Sedna standalone installer"
        );
        assert_eq!(
            update_action_label_for_sedna_identity(
                &windows,
                /*has_sedna_identity*/ true,
                "1.2.3-sedna.4",
                SednaReleaseChannel::Stable,
            ),
            "no automatic update action"
        );
        for release_version in ["1.2.3", "not-a-Sedna-release"] {
            assert_eq!(
                update_action_label_for_sedna_identity(
                    &unix,
                    /*has_sedna_identity*/ true,
                    release_version,
                    SednaReleaseChannel::Stable,
                ),
                "no automatic update action",
                "accepted {release_version}"
            );
        }
        assert_eq!(
            update_action_label_for_sedna_identity(
                &unix,
                /*has_sedna_identity*/ true,
                "1.2.3-alpha.1-sedna.1",
                SednaReleaseChannel::Stable,
            ),
            "Sedna standalone installer"
        );
        for target in [
            ("macos", "x86_64"),
            ("macos", "aarch64"),
            ("freebsd", "x86_64"),
        ] {
            assert_eq!(
                update_action_label_for_sedna_identity_on_target(
                    &unix,
                    /*has_sedna_identity*/ true,
                    "1.2.3-sedna.4",
                    target.0,
                    target.1,
                    SednaReleaseChannel::Stable,
                ),
                "no automatic update action",
                "accepted unsupported target {}-{}",
                target.0,
                target.1
            );
        }
    }

    #[test]
    fn sedna_release_probe_requires_unix_standalone_install() {
        let native_release_dir = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            std::env::temp_dir().join("native-release"),
        )
        .expect("temp dir path should be absolute");
        let unix = InstallContext {
            method: InstallMethod::Standalone {
                platform: StandalonePlatform::Unix,
                release_dir: native_release_dir,
                resources_dir: None,
            },
            package_layout: None,
        };
        let npm = InstallContext {
            method: InstallMethod::Npm,
            package_layout: None,
        };

        assert!(is_sedna_automatic_update_probe_available_on_target(
            &unix,
            /*has_sedna_identity*/ true,
            "1.2.3-sedna.4",
            "linux",
            "x86_64",
            SednaReleaseChannel::Stable,
        ));
        assert!(!is_sedna_automatic_update_probe_available_on_target(
            &npm,
            /*has_sedna_identity*/ true,
            "1.2.3-sedna.4",
            "linux",
            "x86_64",
            SednaReleaseChannel::Stable,
        ));
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
            sedna_release_comparison_status(/*is_newer*/ None),
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
            release_channel: Some(SednaReleaseChannel::Stable),
        };
        let mut details = Vec::new();

        push_cache_info_details(
            &mut details,
            &info,
            /*automatic_update_probe_available*/ true,
            SednaReleaseChannel::Stable,
        );

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
    fn prerelease_doctor_cache_and_metadata_must_match_the_selected_api_channel() {
        let info = VersionInfo {
            latest_version: "1.2.3-alpha.1-sedna.2".to_string(),
            last_checked_at: None,
            dismissed_version: None,
            release_repository: Some("sednalabs/codex".to_string()),
            release_tag_prefix: Some("v".to_string()),
            release_channel: Some(SednaReleaseChannel::Prerelease),
        };
        assert!(info.matches_sedna_release_identity_and_channel(SednaReleaseChannel::Prerelease));
        assert!(!info.matches_sedna_release_identity_and_channel(SednaReleaseChannel::Stable));

        let metadata = ReleaseMetadata {
            release_tag: "v1.2.3-alpha.1-sedna.2".to_string(),
            release_version: "1.2.3-alpha.1-sedna.2".to_string(),
            repository: "sednalabs/codex".to_string(),
            target: "x86_64-unknown-linux-gnu".to_string(),
            release_channel: Some(SednaReleaseChannel::Prerelease),
        };
        assert!(release_metadata_matches(
            &metadata,
            "v1.2.3-alpha.1-sedna.2",
            "1.2.3-alpha.1-sedna.2",
            "x86_64-unknown-linux-gnu",
            SednaReleaseChannel::Prerelease,
        ));
        assert!(!release_metadata_matches(
            &metadata,
            "v1.2.3-alpha.1-sedna.2",
            "1.2.3-alpha.1-sedna.2",
            "x86_64-unknown-linux-gnu",
            SednaReleaseChannel::Stable,
        ));
    }

    #[test]
    fn cached_update_details_require_unix_standalone_install() {
        let info = VersionInfo {
            latest_version: "1.2.3-sedna.2".to_string(),
            last_checked_at: Some("2026-08-20T00:00:00Z".to_string()),
            dismissed_version: Some("1.2.3-sedna.2".to_string()),
            release_repository: Some("sednalabs/codex".to_string()),
            release_tag_prefix: Some("v".to_string()),
            release_channel: Some(SednaReleaseChannel::Stable),
        };
        let native_release_dir = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
            std::env::temp_dir().join("native-release"),
        )
        .expect("temp dir path should be absolute");
        let unix = InstallContext {
            method: InstallMethod::Standalone {
                platform: StandalonePlatform::Unix,
                release_dir: native_release_dir,
                resources_dir: None,
            },
            package_layout: None,
        };
        let npm = InstallContext {
            method: InstallMethod::Npm,
            package_layout: None,
        };

        let mut npm_details = Vec::new();
        let npm_probe_available = is_sedna_automatic_update_probe_available_on_target(
            &npm,
            /*has_sedna_identity*/ true,
            "1.2.3-sedna.4",
            "linux",
            "x86_64",
            SednaReleaseChannel::Stable,
        );
        assert!(!npm_probe_available);
        push_cache_info_details(
            &mut npm_details,
            &info,
            npm_probe_available,
            SednaReleaseChannel::Stable,
        );
        assert!(npm_details.is_empty());

        let mut unix_details = Vec::new();
        let unix_probe_available = is_sedna_automatic_update_probe_available_on_target(
            &unix,
            /*has_sedna_identity*/ true,
            "1.2.3-sedna.4",
            "linux",
            "x86_64",
            SednaReleaseChannel::Stable,
        );
        assert!(unix_probe_available);
        push_cache_info_details(
            &mut unix_details,
            &info,
            unix_probe_available,
            SednaReleaseChannel::Stable,
        );
        assert_eq!(
            unix_details,
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
            release_channel: None,
        };
        let mut details = Vec::new();

        push_cache_info_details(
            &mut details,
            &info,
            /*automatic_update_probe_available*/ true,
            SednaReleaseChannel::Stable,
        );

        assert_eq!(
            details,
            vec!["version cache: ignored (untrusted release identity or channel)"]
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
            release_channel: Some(SednaReleaseChannel::Stable),
        };
        let mut details = Vec::new();

        push_cache_info_details(
            &mut details,
            &info,
            /*automatic_update_probe_available*/ true,
            SednaReleaseChannel::Stable,
        );

        assert_eq!(
            details,
            vec!["version cache: ignored (untrusted release identity or channel)"]
        );
    }
}
