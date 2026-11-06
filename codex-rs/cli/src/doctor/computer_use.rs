use std::path::Path;

use codex_core::config::Config;
use serde::Deserialize;

use super::CheckStatus;
use super::DoctorCheck;
use super::DoctorIssue;
use super::display_list;
use super::env_var_present;
use super::first_present_env;
use super::stdio_command_resolves;

const ANDROID_COMPUTER_USE_CONFIG_FILES: &[&str] = &[
    "android-computer-use.json",
    "android-dynamic-tools.json",
    "solarlab-android-dynamic-tools.json",
];
const ANDROID_COMPUTER_USE_ENV_VARS: &[&str] = &[
    "CODEX_ANDROID_MCP_URL",
    "SOLARLAB_ANDROID_MCP_URL",
    "CODEX_ANDROID_MCP_HOSTNAME",
    "SOLARLAB_ANDROID_MCP_HOSTNAME",
    "CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_ID",
    "SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_ID",
    "CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET",
    "SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET",
];
const ANDROID_MCP_URL_ENV_VARS: &[&str] = &[
    "CODEX_ANDROID_MCP_URL",
    "SOLARLAB_ANDROID_MCP_URL",
    "CODEX_ANDROID_MCP_HOSTNAME",
    "SOLARLAB_ANDROID_MCP_HOSTNAME",
];
const ANDROID_CF_ACCESS_CLIENT_ID_ENV_VARS: &[&str] = &[
    "CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_ID",
    "SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_ID",
];
const ANDROID_CF_ACCESS_CLIENT_SECRET_ENV_VARS: &[&str] = &[
    "CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET",
    "SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET",
];
const DESKTOP_COMPUTER_USE_CONFIG_FILES: &[&str] =
    &["desktop-computer-use.json", "desktop-dynamic-tools.json"];
const DESKTOP_COMPUTER_USE_ENV_VARS: &[&str] = &[
    "CODEX_DESKTOP_COMPUTER_USE_PROVIDER",
    "CODEX_DESKTOP_COMPUTER_USE_COMMAND",
    "CODEX_DESKTOP_COMPUTER_USE_TIMEOUT_SECS",
];
const PROVIDER_NONE: &str = "none";

pub(super) fn check(config: &Config) -> DoctorCheck {
    check_for_home(&config.codex_home)
}

pub(super) fn check_for_home(codex_home: &Path) -> DoctorCheck {
    let mut details = Vec::new();
    let mut issues = Vec::new();
    let android_read_any_config =
        append_android_computer_use_details(codex_home, &mut details, &mut issues);
    let desktop_read_any_config =
        append_desktop_computer_use_details(codex_home, &mut details, &mut issues);

    let status = if issues.is_empty() {
        CheckStatus::Ok
    } else {
        CheckStatus::Warning
    };
    let summary = if android_read_any_config
        || desktop_read_any_config
        || !ANDROID_COMPUTER_USE_ENV_VARS
            .iter()
            .all(|name| !env_var_present(name))
        || !DESKTOP_COMPUTER_USE_ENV_VARS
            .iter()
            .all(|name| !env_var_present(name))
    {
        "native computer-use provider configuration checked"
    } else {
        "native computer-use providers are not configured"
    };
    let mut check = DoctorCheck::new(
        "computer-use.native-provider",
        "computer-use",
        status,
        summary,
    )
    .details(details);
    for issue in issues {
        check = check.issue(issue);
    }
    check
}

fn desktop_provider_configured_from_env() -> bool {
    let command_configured = first_present_env(&["CODEX_DESKTOP_COMPUTER_USE_COMMAND"]).is_some();
    match first_present_env(&["CODEX_DESKTOP_COMPUTER_USE_PROVIDER"]).as_deref() {
        Some(provider) if desktop_provider_is_disabled(provider) => false,
        Some(provider) if !desktop_provider_is_command(provider) => false,
        _ => command_configured,
    }
}

fn desktop_provider_is_disabled(provider: &str) -> bool {
    provider.trim().eq_ignore_ascii_case(PROVIDER_NONE)
}

fn desktop_provider_is_command(provider: &str) -> bool {
    provider.trim().eq_ignore_ascii_case("command")
}

fn append_android_computer_use_details(
    codex_home: &Path,
    details: &mut Vec<String>,
    issues: &mut Vec<DoctorIssue>,
) -> bool {
    let env_overrides = ANDROID_COMPUTER_USE_ENV_VARS
        .iter()
        .copied()
        .filter(|name| env_var_present(name))
        .collect::<Vec<_>>();
    details.push(format!(
        "android provider env overrides: {}",
        display_list(&env_overrides)
    ));

    let mut read_any_config = false;
    let mut configured = first_present_env(ANDROID_MCP_URL_ENV_VARS).is_some();
    for file_name in ANDROID_COMPUTER_USE_CONFIG_FILES {
        let path = codex_home.join(file_name);
        details.push(format!(
            "android provider config {file_name}: {}",
            path.display()
        ));
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                read_any_config = true;
                match serde_json::from_str::<AndroidComputerUseDoctorConfig>(&contents) {
                    Ok(parsed) => {
                        let has_url = parsed
                            .mcp_url
                            .as_deref()
                            .is_some_and(|url| !url.trim().is_empty());
                        configured |= has_url;
                        details.push(format!("android provider config {file_name} parse: ok"));
                        details.push(format!(
                            "android provider config {file_name} mcp_url: {}",
                            if has_url { "configured" } else { "missing" }
                        ));
                    }
                    Err(err) => {
                        issues.push(
                            DoctorIssue::new(
                                CheckStatus::Warning,
                                format!("android provider config {file_name} is not valid JSON"),
                            )
                            .measured(err.to_string())
                            .expected("valid Android computer-use provider configuration")
                            .remedy(format!(
                                "Fix {file_name}, or remove it to disable Android computer-use providers."
                            ))
                            .field(format!("android provider config {file_name} parse")),
                        );
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                details.push(format!(
                    "android provider config {file_name} parse: missing"
                ));
            }
            Err(err) => {
                issues.push(
                    DoctorIssue::new(
                        CheckStatus::Warning,
                        format!("android provider config {file_name} could not be read"),
                    )
                    .measured(err.to_string())
                    .expected("readable Android computer-use provider configuration")
                    .remedy("Check file permissions and rerun codex doctor.")
                    .field(format!("android provider config {file_name} read")),
                );
            }
        }
    }

    let cf_access_client_id = first_present_env(ANDROID_CF_ACCESS_CLIENT_ID_ENV_VARS).is_some();
    let cf_access_client_secret =
        first_present_env(ANDROID_CF_ACCESS_CLIENT_SECRET_ENV_VARS).is_some();
    let cf_access = match (cf_access_client_id, cf_access_client_secret) {
        (true, true) => "configured",
        (false, false) => "not configured",
        (true, false) | (false, true) => "incomplete",
    };
    details.push(format!(
        "android provider cf access credentials: {cf_access}"
    ));
    if cf_access == "incomplete" {
        issues.push(
            DoctorIssue::new(
                CheckStatus::Warning,
                "Android provider Cloudflare Access credentials are incomplete",
            )
            .expected("both client id and client secret, or neither")
            .remedy("Set both Android Cloudflare Access env vars or unset both.")
            .field("android provider cf access credentials"),
        );
    }

    details.push(format!(
        "android providers configured: {}",
        if configured { 1 } else { 0 }
    ));
    if configured {
        details.push(
            "android provider native image contract: MCP image content or android.read_artifact"
                .to_string(),
        );
    }
    read_any_config
}

fn append_desktop_computer_use_details(
    codex_home: &Path,
    details: &mut Vec<String>,
    issues: &mut Vec<DoctorIssue>,
) -> bool {
    let env_overrides = DESKTOP_COMPUTER_USE_ENV_VARS
        .iter()
        .copied()
        .filter(|name| env_var_present(name))
        .collect::<Vec<_>>();
    details.push(format!(
        "desktop provider env overrides: {}",
        display_list(&env_overrides)
    ));
    if let Some(provider) = first_present_env(&["CODEX_DESKTOP_COMPUTER_USE_PROVIDER"]) {
        details.push(format!("desktop provider env provider: {provider}"));
        if !desktop_provider_is_disabled(&provider) && !desktop_provider_is_command(&provider) {
            issues.push(
                DoctorIssue::new(
                    CheckStatus::Warning,
                    "desktop provider env provider kind is unknown",
                )
                .measured(provider)
                .expected("command or none")
                .remedy("Set CODEX_DESKTOP_COMPUTER_USE_PROVIDER=command, or unset it.")
                .field("desktop provider env provider"),
            );
        }
    }

    let mut configured_providers = Vec::new();
    if desktop_provider_configured_from_env() {
        configured_providers.push("env-command".to_string());
    }
    let mut read_any_config = false;
    for file_name in DESKTOP_COMPUTER_USE_CONFIG_FILES {
        let path = codex_home.join(file_name);
        details.push(format!(
            "desktop provider config {file_name}: {}",
            path.display()
        ));
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                read_any_config = true;
                match serde_json::from_str::<DesktopComputerUseDoctorConfig>(&contents) {
                    Ok(parsed) => {
                        let summaries = parsed.provider_summaries();
                        details.push(format!("desktop provider config {file_name} parse: ok"));
                        details.push(format!(
                            "desktop provider config {file_name} providers: {}",
                            summaries.len()
                        ));
                        for provider in summaries {
                            provider.append_details(details);
                            provider.append_issues(issues);
                            if provider.is_configured_for_current_platform() {
                                configured_providers.push(provider.id);
                            }
                        }
                    }
                    Err(err) => {
                        issues.push(
                            DoctorIssue::new(
                                CheckStatus::Warning,
                                format!("desktop provider config {file_name} is not valid JSON"),
                            )
                            .measured(err.to_string())
                            .expected("valid desktop computer-use provider configuration")
                            .remedy(format!(
                                "Fix {file_name}, or remove it to disable desktop computer-use providers."
                            ))
                            .field(format!("desktop provider config {file_name} parse")),
                        );
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                details.push(format!(
                    "desktop provider config {file_name} parse: missing"
                ));
            }
            Err(err) => {
                issues.push(
                    DoctorIssue::new(
                        CheckStatus::Warning,
                        format!("desktop provider config {file_name} could not be read"),
                    )
                    .measured(err.to_string())
                    .expected("readable desktop computer-use provider configuration")
                    .remedy("Check file permissions and rerun codex doctor.")
                    .field(format!("desktop provider config {file_name} read")),
                );
            }
        }
    }

    configured_providers.sort();
    configured_providers.dedup();
    details.push(format!(
        "desktop providers configured: {}",
        configured_providers.len()
    ));
    details.push(format!(
        "desktop provider ids: {}",
        display_list(&configured_providers)
    ));
    read_any_config
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum DoctorCommandSpec {
    String(String),
    Array(Vec<String>),
}

impl DoctorCommandSpec {
    fn argv(&self) -> Option<Vec<String>> {
        let argv = match self {
            Self::String(command) => shlex::split(command)?,
            Self::Array(argv) => argv.clone(),
        };
        argv.first()
            .is_some_and(|program| !program.trim().is_empty())
            .then_some(argv)
    }
}

#[derive(Deserialize)]
struct AndroidComputerUseDoctorConfig {
    mcp_url: Option<String>,
}

#[derive(Deserialize)]
struct DesktopComputerUseDoctorConfig {
    provider: Option<String>,
    command: Option<DoctorCommandSpec>,
    timeout_secs: Option<u64>,
    platforms: Option<Vec<String>>,
    providers: Option<Vec<DesktopDoctorProviderConfig>>,
    routing: Option<DesktopDoctorRoutingConfig>,
}

impl DesktopComputerUseDoctorConfig {
    fn provider_summaries(&self) -> Vec<DesktopProviderDoctorSummary> {
        let mut summaries = Vec::new();
        if self.command.is_some() || self.provider.is_some() {
            let provider = self.provider.as_deref().unwrap_or("command");
            if provider != PROVIDER_NONE {
                summaries.push(DesktopProviderDoctorSummary {
                    id: "legacy".to_string(),
                    provider: provider.to_string(),
                    platforms: self.platforms.clone().unwrap_or_default(),
                    timeout_secs: self.timeout_secs,
                    command: self.command.clone(),
                });
            }
        }
        if let Some(providers) = &self.providers {
            summaries.extend(
                providers
                    .iter()
                    .filter(|provider| {
                        provider.provider.as_deref().unwrap_or("command") != PROVIDER_NONE
                    })
                    .map(DesktopProviderDoctorSummary::from),
            );
        }
        if let Some(order) = self
            .routing
            .as_ref()
            .and_then(|routing| routing.fallback_order.as_ref())
        {
            summaries = order_desktop_provider_summaries(summaries, order);
        }
        summaries
    }
}

#[derive(Deserialize)]
struct DesktopDoctorRoutingConfig {
    fallback_order: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct DesktopDoctorProviderConfig {
    id: Option<String>,
    provider: Option<String>,
    command: Option<DoctorCommandSpec>,
    timeout_secs: Option<u64>,
    platforms: Option<Vec<String>>,
}

fn platform_matches_current(platform: &str) -> bool {
    match platform.trim().to_ascii_lowercase().as_str() {
        "all" | "*" => true,
        "linux" => cfg!(target_os = "linux"),
        "mac" | "macos" | "darwin" => cfg!(target_os = "macos"),
        "windows" | "win32" => cfg!(target_os = "windows"),
        "unix" => cfg!(unix),
        other => other == std::env::consts::OS,
    }
}

#[derive(Clone)]
struct DesktopProviderDoctorSummary {
    id: String,
    provider: String,
    platforms: Vec<String>,
    timeout_secs: Option<u64>,
    command: Option<DoctorCommandSpec>,
}

impl DesktopProviderDoctorSummary {
    fn matches_current_platform(&self) -> bool {
        self.platforms.is_empty()
            || self
                .platforms
                .iter()
                .any(|platform| platform_matches_current(platform))
    }

    fn is_configured_for_current_platform(&self) -> bool {
        self.matches_current_platform()
            && desktop_provider_is_command(&self.provider)
            && self
                .command
                .as_ref()
                .and_then(DoctorCommandSpec::argv)
                .is_some()
    }

    fn append_details(&self, details: &mut Vec<String>) {
        details.push(format!(
            "desktop provider {} kind: {}",
            self.id, self.provider
        ));
        if !self.platforms.is_empty() {
            details.push(format!(
                "desktop provider {} platforms: {}",
                self.id,
                display_list(&self.platforms)
            ));
            details.push(format!(
                "desktop provider {} platform match: {}",
                self.id,
                if self.matches_current_platform() {
                    "yes"
                } else {
                    "no"
                }
            ));
        }
        if let Some(timeout_secs) = self.timeout_secs {
            details.push(format!(
                "desktop provider {} timeout_secs: {}",
                self.id, timeout_secs
            ));
        }
        if let Some(argv) = self.command.as_ref().and_then(DoctorCommandSpec::argv)
            && let Some(program) = argv.first()
        {
            details.push(format!("desktop provider {} command: {}", self.id, program));
            details.push(format!(
                "desktop provider {} command readiness: {}",
                self.id,
                command_readiness(program)
            ));
        }
    }

    fn append_issues(&self, issues: &mut Vec<DoctorIssue>) {
        if !self.matches_current_platform() {
            return;
        }

        if !desktop_provider_is_command(&self.provider) {
            issues.push(
                DoctorIssue::new(
                    CheckStatus::Warning,
                    format!("desktop provider {} has unknown provider kind", self.id),
                )
                .measured(self.provider.clone())
                .expected("command or none")
                .remedy("Use a command provider for desktop computer-use bridges.")
                .field(format!("desktop provider {} kind", self.id)),
            );
            return;
        }

        match self.command.as_ref().and_then(DoctorCommandSpec::argv) {
            Some(argv)
                if argv.first().is_some_and(|program| {
                    stdio_command_resolves(program, /*cwd*/ None, /*server_env*/ None).is_ok()
                }) => {}
            Some(argv) => {
                let program = argv.first().cloned().unwrap_or_default();
                issues.push(
                    DoctorIssue::new(
                        CheckStatus::Warning,
                        format!("desktop provider {} command is not resolvable", self.id),
                    )
                    .measured(program)
                    .expected("command on PATH or executable path")
                    .remedy(
                        "Install the desktop provider command or update desktop-computer-use.json.",
                    )
                    .field(format!("desktop provider {} command readiness", self.id)),
                );
            }
            None => {
                issues.push(
                    DoctorIssue::new(
                        CheckStatus::Warning,
                        format!("desktop provider {} command is missing", self.id),
                    )
                    .expected("non-empty command")
                    .remedy("Add a command array/string for this provider, or disable it.")
                    .field(format!("desktop provider {} command", self.id)),
                );
            }
        }
    }
}

impl From<&DesktopDoctorProviderConfig> for DesktopProviderDoctorSummary {
    fn from(provider: &DesktopDoctorProviderConfig) -> Self {
        let provider_kind = provider
            .provider
            .clone()
            .unwrap_or_else(|| "command".to_string());
        Self {
            id: provider.id.clone().unwrap_or_else(|| provider_kind.clone()),
            provider: provider_kind,
            platforms: provider.platforms.clone().unwrap_or_default(),
            timeout_secs: provider.timeout_secs,
            command: provider.command.clone(),
        }
    }
}

fn order_desktop_provider_summaries(
    providers: Vec<DesktopProviderDoctorSummary>,
    order: &[String],
) -> Vec<DesktopProviderDoctorSummary> {
    let mut remaining = providers;
    let mut ordered = Vec::new();
    for id in order {
        if let Some(index) = remaining.iter().position(|provider| provider.id == *id) {
            ordered.push(remaining.remove(index));
        }
    }
    ordered.extend(remaining);
    ordered
}

fn command_readiness(command: &str) -> String {
    match stdio_command_resolves(command, /*cwd*/ None, /*server_env*/ None) {
        Ok(()) => "resolvable".to_string(),
        Err(err) => format!("not resolvable ({err})"),
    }
}
