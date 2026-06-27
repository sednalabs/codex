use std::path::Path;
use std::process::Command;

use codex_core::config::Config;
use serde::Deserialize;

use super::CheckStatus;
use super::DoctorCheck;
use super::DoctorIssue;
use super::display_list;
use super::env_var_present;
use super::first_present_env;
use super::stdio_command_resolves;

const BROWSER_COMPUTER_USE_CONFIG_FILES: &[&str] =
    &["browser-computer-use.json", "browser-dynamic-tools.json"];
const BROWSER_COMPUTER_USE_ENV_VARS: &[&str] = &[
    "CODEX_BROWSER_COMPUTER_USE_PROVIDER",
    "CODEX_BROWSER_COMPUTER_USE_COMMAND",
    "CODEX_BROWSER_COMPUTER_USE_NODE",
    "CODEX_BROWSER_COMPUTER_USE_TIMEOUT_SECS",
    "CODEX_BROWSER_PLAYWRIGHT_STATE_DIR",
    "CODEX_BROWSER_PLAYWRIGHT_HEADLESS",
    "CODEX_BROWSER_PLAYWRIGHT_NODE_PATH",
    "CODEX_BROWSER_PLAYWRIGHT_EXECUTABLE_PATH",
    "CODEX_BROWSER_PLAYWRIGHT_CHANNEL",
    "CODEX_BROWSER_PLAYWRIGHT_DISPLAY",
    "CODEX_BROWSER_PLAYWRIGHT_CAPTURE_MODE",
    "CODEX_BROWSER_PLAYWRIGHT_VIEWPORT_WIDTH",
    "CODEX_BROWSER_PLAYWRIGHT_VIEWPORT_HEIGHT",
];
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
    let env_overrides = BROWSER_COMPUTER_USE_ENV_VARS
        .iter()
        .copied()
        .filter(|name| env_var_present(name))
        .collect::<Vec<_>>();
    details.push(format!(
        "browser provider env overrides: {}",
        display_list(&env_overrides)
    ));

    let mut configured_providers = Vec::new();
    if browser_provider_configured_from_env() {
        configured_providers.push("env".to_string());
    }
    let mut read_any_config = false;
    for file_name in BROWSER_COMPUTER_USE_CONFIG_FILES {
        let path = codex_home.join(file_name);
        details.push(format!(
            "browser provider config {file_name}: {}",
            path.display()
        ));
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                read_any_config = true;
                match serde_json::from_str::<BrowserComputerUseDoctorConfig>(&contents) {
                    Ok(parsed) => {
                        let summary = parsed.provider_summaries();
                        details.push(format!("browser provider config {file_name} parse: ok"));
                        details.push(format!(
                            "browser provider config {file_name} providers: {}",
                            summary.len()
                        ));
                        for provider in summary {
                            provider.append_details(&mut details);
                            provider.append_issues(&mut issues);
                            configured_providers.push(provider.id);
                        }
                    }
                    Err(err) => {
                        issues.push(
                            DoctorIssue::new(
                                CheckStatus::Warning,
                                format!("browser provider config {file_name} is not valid JSON"),
                            )
                            .measured(err.to_string())
                            .expected("valid browser computer-use provider configuration")
                            .remedy(format!(
                                "Fix {file_name}, or remove it to disable browser computer-use providers."
                            ))
                            .field(format!("browser provider config {file_name} parse")),
                        );
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                details.push(format!(
                    "browser provider config {file_name} parse: missing"
                ));
            }
            Err(err) => {
                issues.push(
                    DoctorIssue::new(
                        CheckStatus::Warning,
                        format!("browser provider config {file_name} could not be read"),
                    )
                    .measured(err.to_string())
                    .expected("readable browser computer-use provider configuration")
                    .remedy("Check file permissions and rerun codex doctor.")
                    .field(format!("browser provider config {file_name} read")),
                );
            }
        }
    }

    configured_providers.sort();
    configured_providers.dedup();
    details.push(format!(
        "browser providers configured: {}",
        configured_providers.len()
    ));
    details.push(format!(
        "browser provider ids: {}",
        display_list(&configured_providers)
    ));

    let android_read_any_config =
        append_android_computer_use_details(codex_home, &mut details, &mut issues);
    let desktop_read_any_config =
        append_desktop_computer_use_details(codex_home, &mut details, &mut issues);

    let status = if issues.is_empty() {
        CheckStatus::Ok
    } else {
        CheckStatus::Warning
    };
    let summary = if read_any_config
        || android_read_any_config
        || desktop_read_any_config
        || !env_overrides.is_empty()
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

fn browser_provider_configured_from_env() -> bool {
    first_present_env(&["CODEX_BROWSER_COMPUTER_USE_COMMAND"]).is_some()
        || first_present_env(&["CODEX_BROWSER_COMPUTER_USE_PROVIDER"])
            .is_some_and(|provider| !provider.eq_ignore_ascii_case("none"))
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

    let mut configured = desktop_provider_configured_from_env();
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
                        let provider = parsed.provider.as_deref().unwrap_or("command");
                        let platform_matches = parsed.matches_current_platform();
                        details.push(format!("desktop provider config {file_name} parse: ok"));
                        details.push(format!(
                            "desktop provider config {file_name} provider: {}",
                            if provider.trim().is_empty() {
                                "command"
                            } else {
                                provider.trim()
                            }
                        ));
                        if let Some(platforms) = &parsed.platforms {
                            details.push(format!(
                                "desktop provider config {file_name} platforms: {}",
                                display_list(platforms)
                            ));
                            details.push(format!(
                                "desktop provider config {file_name} platform match: {}",
                                if platform_matches { "yes" } else { "no" }
                            ));
                        }
                        if let Some(timeout_secs) = parsed.timeout_secs {
                            details.push(format!(
                                "desktop provider config {file_name} timeout_secs: {timeout_secs}"
                            ));
                        }
                        if !platform_matches {
                            continue;
                        }
                        if desktop_provider_is_disabled(provider) {
                            continue;
                        }
                        if !desktop_provider_is_command(provider) {
                            issues.push(
                                DoctorIssue::new(
                                    CheckStatus::Warning,
                                    format!(
                                        "desktop provider config {file_name} has unknown provider kind"
                                    ),
                                )
                                .measured(provider.to_string())
                                .expected("command or none")
                                .remedy("Use a command provider for desktop computer-use bridges.")
                                .field(format!("desktop provider config {file_name} provider")),
                            );
                            continue;
                        }
                        match parsed.command.as_ref().and_then(DoctorCommandSpec::argv) {
                            Some(argv) if !argv.is_empty() => {
                                let program = argv[0].clone();
                                configured = true;
                                details.push(format!(
                                    "desktop provider config {file_name} command: {program}"
                                ));
                                details.push(format!(
                                    "desktop provider config {file_name} command readiness: {}",
                                    command_readiness(&program)
                                ));
                                if stdio_command_resolves(
                                    &program, /*cwd*/ None, /*server_env*/ None,
                                )
                                .is_err()
                                {
                                    issues.push(
                                        DoctorIssue::new(
                                            CheckStatus::Warning,
                                            format!(
                                                "desktop provider config {file_name} command is not resolvable"
                                            ),
                                        )
                                        .measured(program)
                                        .expected("command on PATH or executable path")
                                        .remedy(
                                            "Install the desktop provider command or update desktop-computer-use.json.",
                                        )
                                        .field(format!(
                                            "desktop provider config {file_name} command readiness"
                                        )),
                                    );
                                }
                            }
                            _ if parsed.provider.as_deref() == Some(PROVIDER_NONE) => {}
                            _ => {
                                issues.push(
                                    DoctorIssue::new(
                                        CheckStatus::Warning,
                                        format!(
                                            "desktop provider config {file_name} command is missing"
                                        ),
                                    )
                                    .expected("non-empty command")
                                    .remedy(
                                        "Add a command array/string for this provider, or disable it.",
                                    )
                                    .field(format!("desktop provider config {file_name} command")),
                                );
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

    details.push(format!(
        "desktop providers configured: {}",
        if configured { 1 } else { 0 }
    ));
    read_any_config
}

#[derive(Deserialize)]
struct BrowserComputerUseDoctorConfig {
    provider: Option<String>,
    command: Option<DoctorCommandSpec>,
    node: Option<String>,
    node_path: Option<String>,
    timeout_secs: Option<u64>,
    state_dir: Option<String>,
    headless: Option<bool>,
    executable_path: Option<String>,
    channel: Option<String>,
    display: Option<String>,
    capture_mode: Option<String>,
    viewport_width: Option<u64>,
    viewport_height: Option<u64>,
    providers: Option<Vec<BrowserDoctorProviderConfig>>,
    routing: Option<BrowserDoctorRoutingConfig>,
}

impl BrowserComputerUseDoctorConfig {
    fn provider_summaries(&self) -> Vec<BrowserProviderDoctorSummary> {
        let mut summaries = Vec::new();
        if self.command.is_some() || self.provider.is_some() {
            let provider = self.provider.as_deref().unwrap_or("command");
            if provider != "none" {
                summaries.push(BrowserProviderDoctorSummary {
                    id: "legacy".to_string(),
                    provider: provider.to_string(),
                    backends: if provider == "playwright" {
                        vec!["auto".to_string()]
                    } else {
                        vec!["*".to_string()]
                    },
                    platforms: Vec::new(),
                    timeout_secs: self.timeout_secs,
                    command: self.command.clone(),
                    node: self.node.clone(),
                    node_path: self.node_path.clone(),
                    state_dir: self.state_dir.clone(),
                    headless: self.headless,
                    executable_path: self.executable_path.clone(),
                    channel: self.channel.clone(),
                    display: self.display.clone(),
                    capture_mode: self.capture_mode.clone(),
                    viewport_width: self.viewport_width,
                    viewport_height: self.viewport_height,
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
                    .map(BrowserProviderDoctorSummary::from),
            );
        }
        if let Some(order) = self
            .routing
            .as_ref()
            .and_then(|routing| routing.fallback_order.as_ref())
        {
            summaries = order_provider_summaries(summaries, order);
        }
        summaries
    }
}

#[derive(Deserialize)]
struct BrowserDoctorRoutingConfig {
    fallback_order: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct BrowserDoctorProviderConfig {
    id: Option<String>,
    provider: Option<String>,
    command: Option<DoctorCommandSpec>,
    node: Option<String>,
    node_path: Option<String>,
    timeout_secs: Option<u64>,
    state_dir: Option<String>,
    headless: Option<bool>,
    executable_path: Option<String>,
    channel: Option<String>,
    display: Option<String>,
    capture_mode: Option<String>,
    viewport_width: Option<u64>,
    viewport_height: Option<u64>,
    backends: Option<Vec<String>>,
    platforms: Option<Vec<String>>,
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

#[derive(Clone)]
struct BrowserProviderDoctorSummary {
    id: String,
    provider: String,
    backends: Vec<String>,
    platforms: Vec<String>,
    timeout_secs: Option<u64>,
    command: Option<DoctorCommandSpec>,
    node: Option<String>,
    node_path: Option<String>,
    state_dir: Option<String>,
    headless: Option<bool>,
    executable_path: Option<String>,
    channel: Option<String>,
    display: Option<String>,
    capture_mode: Option<String>,
    viewport_width: Option<u64>,
    viewport_height: Option<u64>,
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
}

impl DesktopComputerUseDoctorConfig {
    fn matches_current_platform(&self) -> bool {
        let Some(platforms) = &self.platforms else {
            return true;
        };
        platforms
            .iter()
            .any(|platform| platform_matches_current(platform))
    }
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

impl BrowserProviderDoctorSummary {
    fn append_details(&self, details: &mut Vec<String>) {
        details.push(format!(
            "browser provider {} kind: {}",
            self.id, self.provider
        ));
        details.push(format!(
            "browser provider {} backends: {}",
            self.id,
            display_list(&self.backends)
        ));
        if !self.platforms.is_empty() {
            details.push(format!(
                "browser provider {} platforms: {}",
                self.id,
                display_list(&self.platforms)
            ));
        }
        if let Some(timeout_secs) = self.timeout_secs {
            details.push(format!(
                "browser provider {} timeout_secs: {}",
                self.id, timeout_secs
            ));
        }
        if let Some(argv) = self.command.as_ref().and_then(DoctorCommandSpec::argv)
            && let Some(program) = argv.first()
        {
            details.push(format!("browser provider {} command: {}", self.id, program));
            details.push(format!(
                "browser provider {} command readiness: {}",
                self.id,
                command_readiness(program)
            ));
        }
        if self.provider == "playwright" {
            let node = self.node.as_deref().unwrap_or("node");
            details.push(format!("browser provider {} node: {}", self.id, node));
            details.push(format!(
                "browser provider {} node readiness: {}",
                self.id,
                command_readiness(node)
            ));
            if let Some(node_path) = &self.node_path {
                details.push(format!(
                    "browser provider {} node_path: {}",
                    self.id, node_path
                ));
            }
            if let Some(state_dir) = &self.state_dir {
                details.push(format!(
                    "browser provider {} state_dir: {}",
                    self.id, state_dir
                ));
            }
            if let Some(headless) = self.headless {
                details.push(format!(
                    "browser provider {} headless: {}",
                    self.id, headless
                ));
            }
            if let Some(executable_path) = &self.executable_path {
                details.push(format!(
                    "browser provider {} executable_path: {}",
                    self.id, executable_path
                ));
                details.push(format!(
                    "browser provider {} executable_path readiness: {}",
                    self.id,
                    command_readiness(executable_path)
                ));
            }
            if let Some(channel) = &self.channel {
                details.push(format!("browser provider {} channel: {}", self.id, channel));
            }
            if let Some(display) = &self.display {
                details.push(format!("browser provider {} display: {}", self.id, display));
            }
            if let Some(capture_mode) = &self.capture_mode {
                details.push(format!(
                    "browser provider {} capture_mode: {}",
                    self.id, capture_mode
                ));
            }
            if let Some(width) = self.viewport_width {
                details.push(format!(
                    "browser provider {} viewport_width: {}",
                    self.id, width
                ));
            }
            if let Some(height) = self.viewport_height {
                details.push(format!(
                    "browser provider {} viewport_height: {}",
                    self.id, height
                ));
            }
        }
    }

    fn append_issues(&self, issues: &mut Vec<DoctorIssue>) {
        match self.provider.as_str() {
            "command" => match self.command.as_ref().and_then(DoctorCommandSpec::argv) {
                Some(argv)
                    if argv.first().is_some_and(|program| {
                        stdio_command_resolves(program, /*cwd*/ None, /*server_env*/ None).is_ok()
                    }) => {}
                Some(argv) => {
                    let program = argv.first().cloned().unwrap_or_default();
                    issues.push(
                        DoctorIssue::new(
                            CheckStatus::Warning,
                            format!("browser provider {} command is not resolvable", self.id),
                        )
                        .measured(program)
                        .expected("command on PATH or executable path")
                        .remedy("Install the provider command or update browser-computer-use.json.")
                        .field(format!("browser provider {} command readiness", self.id)),
                    );
                }
                None => {
                    issues.push(
                        DoctorIssue::new(
                            CheckStatus::Warning,
                            format!("browser provider {} command is missing", self.id),
                        )
                        .expected("non-empty command")
                        .remedy("Add a command array/string for this provider, or disable it.")
                        .field(format!("browser provider {} command", self.id)),
                    );
                }
            },
            "playwright" => {
                let node = self.node.as_deref().unwrap_or("node");
                if stdio_command_resolves(node, /*cwd*/ None, /*server_env*/ None).is_err() {
                    issues.push(
                        DoctorIssue::new(
                            CheckStatus::Warning,
                            format!(
                                "browser provider {} node command is not resolvable",
                                self.id
                            ),
                        )
                        .measured(node.to_string())
                        .expected("node command on PATH or executable path")
                        .remedy("Install Node.js or configure CODEX_BROWSER_COMPUTER_USE_NODE.")
                        .field(format!("browser provider {} node readiness", self.id)),
                    );
                } else if let Err(err) = playwright_module_resolves(node, self.node_path.as_deref())
                {
                    issues.push(
                        DoctorIssue::new(
                            CheckStatus::Warning,
                            format!(
                                "browser provider {} Playwright package is not resolvable",
                                self.id
                            ),
                        )
                        .measured(err)
                        .expected("Node can resolve the playwright package")
                        .remedy(
                            "Install Playwright where Node can resolve it, or set browser provider node_path.",
                        )
                        .field(format!(
                            "browser provider {} playwright module readiness",
                            self.id
                        )),
                    );
                } else if self.executable_path.is_none()
                    && self.channel.is_none()
                    && let Err(err) =
                        playwright_default_browser_resolves(node, self.node_path.as_deref())
                {
                    issues.push(
                        DoctorIssue::new(
                            CheckStatus::Warning,
                            format!(
                                "browser provider {} Playwright browser executable is missing",
                                self.id
                            ),
                        )
                        .measured(err)
                        .expected("installed Playwright browser cache or explicit executable_path/channel")
                        .remedy(
                            "Run `npx playwright install chromium`, or set browser provider executable_path to Chrome/Chromium.",
                        )
                        .field(format!(
                            "browser provider {} playwright browser readiness",
                            self.id
                        )),
                    );
                }
                if let Some(executable_path) = &self.executable_path
                    && stdio_command_resolves(
                        executable_path,
                        /*cwd*/ None,
                        /*server_env*/ None,
                    )
                    .is_err()
                {
                    issues.push(
                        DoctorIssue::new(
                            CheckStatus::Warning,
                            format!(
                                "browser provider {} executable path is not resolvable",
                                self.id
                            ),
                        )
                        .measured(executable_path.clone())
                        .expected("Google Chrome/Chromium executable path")
                        .remedy("Install Chrome/Chromium or update browser-computer-use.json.")
                        .field(format!(
                            "browser provider {} executable_path readiness",
                            self.id
                        )),
                    );
                }
                if self.headless == Some(false)
                    && self
                        .display
                        .as_deref()
                        .map(|display| display.trim().is_empty())
                        .unwrap_or(true)
                    && std::env::var("DISPLAY")
                        .ok()
                        .map(|display| display.trim().is_empty())
                        .unwrap_or(true)
                {
                    issues.push(
                        DoctorIssue::new(
                            CheckStatus::Warning,
                            format!(
                                "browser provider {} is headed but no display is configured",
                                self.id
                            ),
                        )
                        .expected("DISPLAY or browser provider display")
                        .remedy(
                            "Start a visible X11/noVNC session and set display in browser-computer-use.json.",
                        )
                        .field(format!("browser provider {} display", self.id)),
                    );
                }
                if let Some(capture_mode) = &self.capture_mode
                    && !matches!(
                        capture_mode.trim().to_ascii_lowercase().as_str(),
                        "viewport" | "full_page"
                    )
                {
                    issues.push(
                        DoctorIssue::new(
                            CheckStatus::Warning,
                            format!("browser provider {} capture mode is unknown", self.id),
                        )
                        .measured(capture_mode.clone())
                        .expected("viewport or full_page")
                        .remedy("Use capture_mode=viewport for realistic editor UX loops.")
                        .field(format!("browser provider {} capture_mode", self.id)),
                    );
                }
            }
            "none" => {}
            other => {
                issues.push(
                    DoctorIssue::new(
                        CheckStatus::Warning,
                        format!("browser provider {} has unknown provider kind", self.id),
                    )
                    .measured(other.to_string())
                    .expected("command, playwright, or none")
                    .remedy("Use a command provider for native/browser shell bridges.")
                    .field(format!("browser provider {} kind", self.id)),
                );
            }
        }
    }
}

impl From<&BrowserDoctorProviderConfig> for BrowserProviderDoctorSummary {
    fn from(provider: &BrowserDoctorProviderConfig) -> Self {
        let provider_kind = provider
            .provider
            .clone()
            .unwrap_or_else(|| "command".to_string());
        Self {
            id: provider.id.clone().unwrap_or_else(|| provider_kind.clone()),
            provider: provider_kind.clone(),
            backends: provider.backends.clone().unwrap_or_else(|| {
                if provider_kind == "playwright" {
                    vec!["auto".to_string()]
                } else {
                    vec!["*".to_string()]
                }
            }),
            platforms: provider.platforms.clone().unwrap_or_default(),
            timeout_secs: provider.timeout_secs,
            command: provider.command.clone(),
            node: provider.node.clone(),
            node_path: provider.node_path.clone(),
            state_dir: provider.state_dir.clone(),
            headless: provider.headless,
            executable_path: provider.executable_path.clone(),
            channel: provider.channel.clone(),
            display: provider.display.clone(),
            capture_mode: provider.capture_mode.clone(),
            viewport_width: provider.viewport_width,
            viewport_height: provider.viewport_height,
        }
    }
}

fn order_provider_summaries(
    providers: Vec<BrowserProviderDoctorSummary>,
    order: &[String],
) -> Vec<BrowserProviderDoctorSummary> {
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

fn playwright_module_resolves(node: &str, node_path: Option<&str>) -> Result<String, String> {
    let output = node_command(node, node_path)
        .arg("-e")
        .arg("process.stdout.write(require.resolve('playwright'))")
        .output()
        .map_err(|err| format!("failed to run node: {err}"))?;
    command_success_output(output, "resolve playwright package")
}

fn playwright_default_browser_resolves(
    node: &str,
    node_path: Option<&str>,
) -> Result<String, String> {
    let script = r#"
const fs = require('fs');
const { chromium } = require('playwright');
const executablePath = chromium.executablePath();
process.stdout.write(executablePath);
if (!fs.existsSync(executablePath)) {
  process.exitCode = 2;
}
"#;
    let output = node_command(node, node_path)
        .arg("-e")
        .arg(script)
        .output()
        .map_err(|err| format!("failed to run node: {err}"))?;
    command_success_output(output, "resolve Playwright Chromium executable")
}

fn node_command(node: &str, node_path: Option<&str>) -> Command {
    let mut command = Command::new(node);
    if let Some(node_path) = node_path
        && !node_path.trim().is_empty()
    {
        command.env("NODE_PATH", node_path);
    }
    command
}

fn command_success_output(output: std::process::Output, context: &str) -> Result<String, String> {
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() {
        return Ok(stdout);
    }

    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let measured = if stdout.is_empty() {
        stderr
    } else if stderr.is_empty() {
        stdout
    } else {
        format!("{stdout}: {stderr}")
    };
    Err(format!(
        "{context} failed with status {}: {}",
        output.status,
        if measured.is_empty() {
            "<no output>".to_string()
        } else {
            measured
        }
    ))
}
