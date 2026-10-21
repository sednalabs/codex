//! Browser native computer-use provider shared by interactive and exec
//! frontends.

use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolNamespaceSpec;
use codex_app_server_protocol::DynamicToolNamespaceTool;
use codex_app_server_protocol::DynamicToolSpec;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use serde::Deserialize;
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::time::timeout;

mod browser_sessions;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const ENV_PROVIDER: &str = "CODEX_BROWSER_COMPUTER_USE_PROVIDER";
const ENV_COMMAND: &str = "CODEX_BROWSER_COMPUTER_USE_COMMAND";
const ENV_NODE: &str = "CODEX_BROWSER_COMPUTER_USE_NODE";
const ENV_TIMEOUT_SECS: &str = "CODEX_BROWSER_COMPUTER_USE_TIMEOUT_SECS";
const ENV_PLAYWRIGHT_STATE_DIR: &str = "CODEX_BROWSER_PLAYWRIGHT_STATE_DIR";
const ENV_PLAYWRIGHT_HEADLESS: &str = "CODEX_BROWSER_PLAYWRIGHT_HEADLESS";
const ENV_PLAYWRIGHT_NODE_PATH: &str = "CODEX_BROWSER_PLAYWRIGHT_NODE_PATH";
const ENV_PLAYWRIGHT_EXECUTABLE_PATH: &str = "CODEX_BROWSER_PLAYWRIGHT_EXECUTABLE_PATH";
const ENV_PLAYWRIGHT_CHANNEL: &str = "CODEX_BROWSER_PLAYWRIGHT_CHANNEL";
const ENV_PLAYWRIGHT_DISPLAY: &str = "CODEX_BROWSER_PLAYWRIGHT_DISPLAY";
const ENV_PLAYWRIGHT_VIEWPORT_WIDTH: &str = "CODEX_BROWSER_PLAYWRIGHT_VIEWPORT_WIDTH";
const ENV_PLAYWRIGHT_VIEWPORT_HEIGHT: &str = "CODEX_BROWSER_PLAYWRIGHT_VIEWPORT_HEIGHT";
const PROVIDER_COMMAND: &str = "command";
const PROVIDER_NONE: &str = "none";
const PROVIDER_PLAYWRIGHT: &str = "playwright";
const TOOL_BROWSER_OBSERVE: &str = "browser_observe";
const TOOL_BROWSER_STEP: &str = "browser_step";
const BACKEND_AUTO: &str = "auto";
const BACKEND_BROWSER: &str = "browser";
const BACKEND_CHROME: &str = "chrome";
const BACKEND_CHROMIUM: &str = "chromium";
const BACKEND_WILDCARD: &str = "*";

const PLAYWRIGHT_BRIDGE_SCRIPT: &str = include_str!("browser_playwright_provider.mjs");

/// Return browser computer-use dynamic tools for a specific Codex home.
pub fn configured_browser_dynamic_tools_for_codex_home(codex_home: &Path) -> Vec<DynamicToolSpec> {
    if BrowserRuntimeConfig::load(codex_home).is_none() {
        return Vec::new();
    }

    vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
        name: "codex_browser".to_string(),
        description: "Native Browser tools".to_string(),
        tools: vec![
            browser_dynamic_tool(
                TOOL_BROWSER_OBSERVE,
                "Capture the current browser viewport as a model-visible screenshot.",
            ),
            browser_dynamic_tool(
                TOOL_BROWSER_STEP,
                "Perform one or more bounded browser actions, then return one fresh viewport screenshot.",
            ),
        ],
    })]
}

fn browser_dynamic_tool(name: &str, description: &str) -> DynamicToolNamespaceTool {
    DynamicToolNamespaceTool::Function(DynamicToolFunctionSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": true,
            "properties": {
                "backend": {
                    "type": "string",
                    "enum": ["auto", "browser", "chrome", "chromium"],
                    "description": "Select a configured Browser backend."
                },
                "url": {"type": "string", "description": "Optional page URL to open before observing or acting."},
                "timeout_secs": {"type": "integer", "minimum": 1, "description": "Optional navigation timeout in seconds."},
                "action": {"type": "string", "description": "One browser action type, with its action-specific arguments."},
                "type": {"type": "string", "description": "One browser action type, with its action-specific arguments."},
                "actions": {
                    "type": "array",
                    "description": "Ordered browser actions; each action object uses a type and action-specific arguments.",
                    "items": {"type": "object", "additionalProperties": true}
                }
            }
        }),
        defer_loading: false,
    })
}

/// Result of trying to route a browser computer-use request to a configured
/// local provider.
pub enum BrowserComputerUseOutcome {
    /// The request was claimed by the browser provider path and converted into
    /// a model-facing response.
    Handled(DynamicToolCallResponse),
    /// The request is for another adapter/tool or browser use is not
    /// configured for the active Codex home.
    Unavailable,
}

/// Handle a browser computer-use request using a specific Codex home.
pub async fn handle_browser_computer_use_for_codex_home(
    params: &DynamicToolCallParams,
    codex_home: &Path,
) -> BrowserComputerUseOutcome {
    if params.namespace.as_deref() != Some("codex_browser")
        || !matches!(
            params.tool.as_str(),
            TOOL_BROWSER_OBSERVE | TOOL_BROWSER_STEP
        )
    {
        return BrowserComputerUseOutcome::Unavailable;
    }

    let Some(config) = BrowserRuntimeConfig::load(codex_home) else {
        return BrowserComputerUseOutcome::Unavailable;
    };

    let requested_backend = requested_backend(&params.arguments);
    let Some(provider) = config.provider_for_backend(requested_backend).cloned() else {
        return BrowserComputerUseOutcome::Handled(failed_response(format!(
            "Browser backend `{requested_backend}` is not available from configured browser computer-use providers. Configure `{ENV_COMMAND}` or add a matching provider to ~/.codex/browser-computer-use.json."
        )));
    };

    let request_timeout = provider.timeout;
    let response = match timeout(request_timeout, handle_with_provider(params, provider)).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => failed_response(err),
        Err(_) => failed_response(format!(
            "Browser computer-use provider timed out after {} seconds.",
            request_timeout.as_secs()
        )),
    };
    BrowserComputerUseOutcome::Handled(response)
}

async fn handle_with_provider(
    params: &DynamicToolCallParams,
    provider: ConfiguredBrowserProvider,
) -> Result<DynamicToolCallResponse, String> {
    let mut response = match provider.provider {
        BrowserProvider::Command(command) => run_command_provider(params, &command).await,
        BrowserProvider::Playwright(playwright) => {
            run_playwright_provider(params, &playwright).await
        }
    }?;

    require_native_image_for_visual_response(
        &mut response,
        "Browser observation missing native image output.",
    );
    Ok(response)
}

async fn run_command_provider(
    params: &DynamicToolCallParams,
    command: &CommandProviderConfig,
) -> Result<DynamicToolCallResponse, String> {
    let output = run_provider_process(&command.argv, params, &[]).await?;
    parse_provider_response(&output)
}

async fn run_playwright_provider(
    params: &DynamicToolCallParams,
    config: &PlaywrightProviderConfig,
) -> Result<DynamicToolCallResponse, String> {
    browser_sessions::handle(params, config).await
}

/// Return whether the process-scoped native Browser manager currently owns a
/// session for this thread. TUI uses this to decide whether a ThreadClosed
/// notification needs the same authoritative readback used for the displayed
/// thread before releasing its retained page.
pub async fn has_browser_session_for_thread(thread_id: &str) -> bool {
    browser_sessions::has_session_for_thread(thread_id).await
}

/// Close a retained Browser session only after the caller has confirmed the
/// thread is unloaded.
pub async fn close_browser_thread(thread_id: &str) {
    browser_sessions::close_thread(thread_id).await;
}

/// Gracefully close process-owned Browser sessions before frontend shutdown.
pub async fn shutdown_browser_sessions() {
    browser_sessions::shutdown_all().await;
}

fn playwright_provider_envs(config: &PlaywrightProviderConfig) -> Vec<(String, String)> {
    let mut envs = Vec::new();
    push_env(
        &mut envs,
        ENV_PLAYWRIGHT_NODE_PATH,
        config.node_path.clone(),
    );
    if let Some(node_path) = &config.node_path {
        push_env(&mut envs, "NODE_PATH", Some(node_path.clone()));
    }
    push_env(
        &mut envs,
        ENV_PLAYWRIGHT_STATE_DIR,
        config.state_dir.clone(),
    );
    push_env(
        &mut envs,
        ENV_PLAYWRIGHT_EXECUTABLE_PATH,
        config.executable_path.clone(),
    );
    push_env(&mut envs, ENV_PLAYWRIGHT_CHANNEL, config.channel.clone());
    push_env(&mut envs, ENV_PLAYWRIGHT_DISPLAY, config.display.clone());
    if let Some(display) = &config.display {
        push_env(&mut envs, "DISPLAY", Some(display.clone()));
    }
    push_env(
        &mut envs,
        ENV_PLAYWRIGHT_VIEWPORT_WIDTH,
        config.viewport_width.map(|width| width.to_string()),
    );
    push_env(
        &mut envs,
        ENV_PLAYWRIGHT_VIEWPORT_HEIGHT,
        config.viewport_height.map(|height| height.to_string()),
    );
    if let Some(headless) = config.headless {
        push_env(
            &mut envs,
            ENV_PLAYWRIGHT_HEADLESS,
            Some(if headless { "1" } else { "0" }.to_string()),
        );
    }
    envs
}

fn push_env(envs: &mut Vec<(String, String)>, key: &str, value: Option<String>) {
    if let Some(value) = value
        && !value.trim().is_empty()
    {
        envs.push((key.to_string(), value));
    }
}

async fn run_provider_process(
    argv: &[String],
    params: &DynamicToolCallParams,
    envs: &[(String, String)],
) -> Result<Vec<u8>, String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "Browser provider command is empty.".to_string())?;
    let mut child = Command::new(program)
        .args(args)
        .envs(envs.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("failed to start browser provider `{program}`: {err}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open browser provider stdin".to_string())?;
    let body = serde_json::to_vec(params)
        .map_err(|err| format!("failed to serialize browser provider request: {err}"))?;
    stdin
        .write_all(&body)
        .await
        .map_err(|err| format!("failed to write browser provider request: {err}"))?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .map_err(|err| format!("failed to wait for browser provider: {err}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "Browser provider exited with status {}.",
            output.status
        ))
    }
}

fn parse_provider_response(bytes: &[u8]) -> Result<DynamicToolCallResponse, String> {
    serde_json::from_slice(bytes).map_err(|error| {
        format!(
            "failed to parse browser provider response ({} error at line {}, column {}).",
            match error.classify() {
                serde_json::error::Category::Io => "I/O",
                serde_json::error::Category::Syntax => "syntax",
                serde_json::error::Category::Data => "data",
                serde_json::error::Category::Eof => "end-of-input",
            },
            error.line(),
            error.column(),
        )
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BrowserRuntimeConfig {
    providers: Vec<ConfiguredBrowserProvider>,
}

impl BrowserRuntimeConfig {
    fn load(codex_home: &Path) -> Option<Self> {
        Self::from_sources(
            BrowserRuntimeConfigFile::load(codex_home),
            BrowserRuntimeEnv::read(),
        )
    }

    fn from_sources(
        file: Option<BrowserRuntimeConfigFile>,
        env: BrowserRuntimeEnv,
    ) -> Option<Self> {
        let provider_name = env
            .provider
            .clone()
            .or_else(|| file.as_ref().and_then(|config| config.provider.clone()));
        if provider_name.as_deref() == Some(PROVIDER_NONE) {
            return None;
        }

        let timeout = env
            .timeout_secs
            .or_else(|| file.as_ref().and_then(|config| config.timeout_secs))
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT);

        if let Some(command) = env
            .command
            .clone()
            .and_then(|command| command_spec_to_argv(CommandSpec::String(command)))
            .or_else(|| {
                file.as_ref()
                    .and_then(|config| config.command.clone())
                    .and_then(command_spec_to_argv)
            })
        {
            return Some(Self {
                providers: vec![ConfiguredBrowserProvider::command(
                    "env-command",
                    command,
                    wildcard_backends(),
                    timeout,
                )],
            });
        }

        if provider_name.as_deref() == Some(PROVIDER_PLAYWRIGHT) {
            return Some(Self {
                providers: vec![ConfiguredBrowserProvider::playwright(
                    "playwright",
                    PlaywrightProviderConfig {
                        node: env
                            .node
                            .clone()
                            .or_else(|| file.as_ref().and_then(|config| config.node.clone()))
                            .unwrap_or_else(|| "node".to_string()),
                        node_path: env
                            .node_path
                            .clone()
                            .or_else(|| file.as_ref().and_then(|config| config.node_path.clone())),
                        state_dir: env
                            .state_dir
                            .clone()
                            .or_else(|| file.as_ref().and_then(|config| config.state_dir.clone())),
                        headless: env
                            .headless
                            .or_else(|| file.as_ref().and_then(|config| config.headless)),
                        executable_path: env.executable_path.clone().or_else(|| {
                            file.as_ref()
                                .and_then(|config| config.executable_path.clone())
                        }),
                        channel: env
                            .channel
                            .clone()
                            .or_else(|| file.as_ref().and_then(|config| config.channel.clone())),
                        display: env
                            .display
                            .clone()
                            .or_else(|| file.as_ref().and_then(|config| config.display.clone())),
                        viewport_width: env
                            .viewport_width
                            .or_else(|| file.as_ref().and_then(|config| config.viewport_width)),
                        viewport_height: env
                            .viewport_height
                            .or_else(|| file.as_ref().and_then(|config| config.viewport_height)),
                    },
                    default_playwright_backends(),
                    timeout,
                )],
            });
        }

        if provider_name.as_deref() == Some(PROVIDER_COMMAND) {
            return None;
        }

        let file = file?;
        let providers = file.configured_providers(timeout, &env);
        (!providers.is_empty()).then_some(Self { providers })
    }

    fn provider_for_backend(&self, backend: &str) -> Option<&ConfiguredBrowserProvider> {
        self.providers
            .iter()
            .find(|provider| provider.supports_backend(backend))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfiguredBrowserProvider {
    id: String,
    provider: BrowserProvider,
    backends: Vec<String>,
    timeout: Duration,
}

impl ConfiguredBrowserProvider {
    fn command(
        id: impl Into<String>,
        argv: Vec<String>,
        backends: Vec<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            id: id.into(),
            provider: BrowserProvider::Command(CommandProviderConfig { argv }),
            backends,
            timeout,
        }
    }

    fn playwright(
        id: impl Into<String>,
        config: PlaywrightProviderConfig,
        backends: Vec<String>,
        timeout: Duration,
    ) -> Self {
        Self {
            id: id.into(),
            provider: BrowserProvider::Playwright(config),
            backends,
            timeout,
        }
    }

    fn supports_backend(&self, backend: &str) -> bool {
        self.backends.is_empty()
            || self.backends.iter().any(|configured| {
                configured == BACKEND_WILDCARD || configured.eq_ignore_ascii_case(backend)
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BrowserProvider {
    Command(CommandProviderConfig),
    Playwright(PlaywrightProviderConfig),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandProviderConfig {
    argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaywrightProviderConfig {
    node: String,
    node_path: Option<String>,
    state_dir: Option<String>,
    headless: Option<bool>,
    executable_path: Option<String>,
    channel: Option<String>,
    display: Option<String>,
    viewport_width: Option<u64>,
    viewport_height: Option<u64>,
}

#[derive(Deserialize)]
struct BrowserRuntimeConfigFile {
    provider: Option<String>,
    command: Option<CommandSpec>,
    node: Option<String>,
    node_path: Option<String>,
    timeout_secs: Option<u64>,
    state_dir: Option<String>,
    headless: Option<bool>,
    executable_path: Option<String>,
    channel: Option<String>,
    display: Option<String>,
    viewport_width: Option<u64>,
    viewport_height: Option<u64>,
    providers: Option<Vec<BrowserProviderConfigFile>>,
    routing: Option<BrowserRoutingConfigFile>,
}

impl BrowserRuntimeConfigFile {
    fn load(codex_home: &Path) -> Option<Self> {
        for path in [
            codex_home.join("browser-computer-use.json"),
            codex_home.join("browser-dynamic-tools.json"),
        ] {
            if let Ok(contents) = std::fs::read_to_string(path)
                && let Ok(config) = serde_json::from_str(&contents)
            {
                return Some(config);
            }
        }
        None
    }

    fn configured_providers(
        &self,
        default_timeout: Duration,
        env: &BrowserRuntimeEnv,
    ) -> Vec<ConfiguredBrowserProvider> {
        let mut providers = self
            .providers
            .as_ref()
            .map(|providers| {
                providers
                    .iter()
                    .filter(|provider| provider.matches_current_platform())
                    .filter_map(|provider| provider.to_configured(default_timeout, env))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        if let Some(order) = self
            .routing
            .as_ref()
            .and_then(|routing| routing.fallback_order.as_ref())
        {
            providers = order_providers(providers, order);
        }

        providers
    }
}

#[derive(Deserialize)]
struct BrowserRoutingConfigFile {
    fallback_order: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct BrowserProviderConfigFile {
    id: Option<String>,
    provider: Option<String>,
    command: Option<CommandSpec>,
    node: Option<String>,
    node_path: Option<String>,
    timeout_secs: Option<u64>,
    state_dir: Option<String>,
    headless: Option<bool>,
    executable_path: Option<String>,
    channel: Option<String>,
    display: Option<String>,
    viewport_width: Option<u64>,
    viewport_height: Option<u64>,
    backends: Option<Vec<String>>,
    platforms: Option<Vec<String>>,
}

impl BrowserProviderConfigFile {
    fn matches_current_platform(&self) -> bool {
        let Some(platforms) = &self.platforms else {
            return true;
        };
        platforms
            .iter()
            .any(|platform| platform_matches_current(platform))
    }

    fn to_configured(
        &self,
        default_timeout: Duration,
        env: &BrowserRuntimeEnv,
    ) -> Option<ConfiguredBrowserProvider> {
        let provider_name = self.provider.as_deref().unwrap_or(PROVIDER_COMMAND);
        if provider_name == PROVIDER_NONE {
            return None;
        }

        let timeout = self
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(default_timeout);
        let backends = self.backends.clone().unwrap_or_else(|| {
            if provider_name == PROVIDER_PLAYWRIGHT {
                default_playwright_backends()
            } else {
                wildcard_backends()
            }
        });
        let id = self.id.clone().unwrap_or_else(|| provider_name.to_string());

        match provider_name {
            PROVIDER_PLAYWRIGHT => Some(ConfiguredBrowserProvider::playwright(
                id,
                PlaywrightProviderConfig {
                    node: self
                        .node
                        .clone()
                        .or_else(|| env.node.clone())
                        .unwrap_or_else(|| "node".to_string()),
                    node_path: self.node_path.clone().or_else(|| env.node_path.clone()),
                    state_dir: self.state_dir.clone().or_else(|| env.state_dir.clone()),
                    headless: self.headless.or(env.headless),
                    executable_path: self
                        .executable_path
                        .clone()
                        .or_else(|| env.executable_path.clone()),
                    channel: self.channel.clone().or_else(|| env.channel.clone()),
                    display: self.display.clone().or_else(|| env.display.clone()),
                    viewport_width: self.viewport_width.or(env.viewport_width),
                    viewport_height: self.viewport_height.or(env.viewport_height),
                },
                backends,
                timeout,
            )),
            PROVIDER_COMMAND => self
                .command
                .clone()
                .and_then(command_spec_to_argv)
                .map(|argv| ConfiguredBrowserProvider::command(id, argv, backends, timeout)),
            _ => None,
        }
    }
}

#[derive(Default)]
struct BrowserRuntimeEnv {
    provider: Option<String>,
    command: Option<String>,
    node: Option<String>,
    node_path: Option<String>,
    timeout_secs: Option<u64>,
    state_dir: Option<String>,
    headless: Option<bool>,
    executable_path: Option<String>,
    channel: Option<String>,
    display: Option<String>,
    viewport_width: Option<u64>,
    viewport_height: Option<u64>,
}

impl BrowserRuntimeEnv {
    fn read() -> Self {
        Self {
            provider: first_env(&[ENV_PROVIDER]),
            command: first_env(&[ENV_COMMAND]),
            node: first_env(&[ENV_NODE]),
            node_path: first_env(&[ENV_PLAYWRIGHT_NODE_PATH]),
            timeout_secs: first_env(&[ENV_TIMEOUT_SECS]).and_then(|value| value.parse().ok()),
            state_dir: first_env(&[ENV_PLAYWRIGHT_STATE_DIR]),
            headless: first_env(&[ENV_PLAYWRIGHT_HEADLESS]).and_then(|value| parse_bool(&value)),
            executable_path: first_env(&[ENV_PLAYWRIGHT_EXECUTABLE_PATH]),
            channel: first_env(&[ENV_PLAYWRIGHT_CHANNEL]),
            display: first_env(&[ENV_PLAYWRIGHT_DISPLAY]),
            viewport_width: first_env(&[ENV_PLAYWRIGHT_VIEWPORT_WIDTH])
                .and_then(|value| value.parse().ok()),
            viewport_height: first_env(&[ENV_PLAYWRIGHT_VIEWPORT_HEIGHT])
                .and_then(|value| value.parse().ok()),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(untagged)]
enum CommandSpec {
    String(String),
    Array(Vec<String>),
}

fn command_spec_to_argv(command: CommandSpec) -> Option<Vec<String>> {
    let argv = match command {
        CommandSpec::String(command) => shlex::split(&command)?,
        CommandSpec::Array(argv) => argv,
    };
    (!argv.is_empty()).then_some(argv)
}

fn order_providers(
    providers: Vec<ConfiguredBrowserProvider>,
    order: &[String],
) -> Vec<ConfiguredBrowserProvider> {
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

fn wildcard_backends() -> Vec<String> {
    vec![BACKEND_WILDCARD.to_string()]
}

fn default_playwright_backends() -> Vec<String> {
    vec![
        BACKEND_AUTO.to_string(),
        BACKEND_BROWSER.to_string(),
        BACKEND_CHROME.to_string(),
        BACKEND_CHROMIUM.to_string(),
    ]
}

fn platform_matches_current(platform: &str) -> bool {
    match platform.to_ascii_lowercase().as_str() {
        "all" | "*" => true,
        "linux" => cfg!(target_os = "linux"),
        "mac" | "macos" | "darwin" => cfg!(target_os = "macos"),
        "windows" | "win32" => cfg!(target_os = "windows"),
        "unix" => cfg!(unix),
        other => other == std::env::consts::OS,
    }
}

fn requested_backend(arguments: &Value) -> &str {
    arguments
        .get("backend")
        .and_then(Value::as_str)
        .unwrap_or(BACKEND_AUTO)
}

fn first_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.trim().is_empty())
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn response_includes_native_image(response: &DynamicToolCallResponse) -> bool {
    response
        .content_items
        .iter()
        .any(|item| matches!(item, DynamicToolCallOutputContentItem::InputImage { .. }))
}

fn require_native_image_for_visual_response(
    response: &mut DynamicToolCallResponse,
    missing_image_message: &str,
) {
    if !response.success || response_includes_native_image(response) {
        return;
    }

    append_text(
        &mut response.content_items,
        &format!(
            "\n\n{missing_image_message} The browser provider must return screenshots as native image content items."
        ),
    );
    response.success = false;
}

fn append_text(items: &mut Vec<DynamicToolCallOutputContentItem>, extra: &str) {
    if let Some(DynamicToolCallOutputContentItem::InputText { text }) = items.first_mut() {
        text.push_str(extra);
    } else {
        items.insert(
            0,
            DynamicToolCallOutputContentItem::InputText {
                text: extra.trim().to_string(),
            },
        );
    }
}

fn failed_response(error: String) -> DynamicToolCallResponse {
    DynamicToolCallResponse {
        content_items: vec![DynamicToolCallOutputContentItem::InputText { text: error }],
        success: false,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use super::browser_sessions::BrowserSessionManager;
    #[cfg(unix)]
    use super::*;

    #[cfg(unix)]
    fn fake_provider_config(fixture_root: &Path, state_dir: &Path) -> PlaywrightProviderConfig {
        let node_path = fixture_root.join("node-modules");
        let playwright = node_path.join("playwright");
        std::fs::create_dir_all(&playwright).expect("fake Playwright module directory");
        std::fs::write(
            playwright.join("index.js"),
            r#"
const fs = require("node:fs");
const path = require("node:path");
const root = process.env.CODEX_BROWSER_PLAYWRIGHT_STATE_DIR;
const marker = (name) => path.join(root, name);
let currentUrl = "about:blank";
let currentText = "";
let pendingFillResolve = null;
const page = {
  isClosed: () => false,
  url: () => currentUrl,
  goto: async (url) => { currentUrl = url; },
  waitForLoadState: async () => {},
  screenshot: async () => Buffer.from("synthetic-browser-image"),
  title: async () => currentText || "fixture page",
  locator: () => {
    const locator = {
      first: () => locator,
      fill: async (text) => {
        if (fs.existsSync(marker("pause-action"))) {
          fs.writeFileSync(marker("action-started"), "started");
          await new Promise((resolve) => { pendingFillResolve = resolve; });
        }
        currentText = text;
      },
    };
    return locator;
  },
  viewportSize: () => ({ width: 1280, height: 720 }),
};
const context = {
  pages: () => [],
  newPage: async () => page,
  close: async () => {
    fs.writeFileSync(marker("context-closed"), "closed");
    if (pendingFillResolve) {
      pendingFillResolve();
      pendingFillResolve = null;
    }
    if (fs.existsSync(marker("hang-close"))) {
      await new Promise(() => {});
    }
  },
};
exports.chromium = {
  launchPersistentContext: async () => {
    if (fs.existsSync(marker("pause-launch"))) {
      fs.writeFileSync(marker("launch-started"), "started");
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
    return context;
  },
};
"#,
        )
        .expect("fake Playwright module source");

        PlaywrightProviderConfig {
            node: "node".to_string(),
            node_path: Some(node_path.to_string_lossy().into_owned()),
            state_dir: Some(state_dir.to_string_lossy().into_owned()),
            headless: Some(true),
            executable_path: None,
            channel: None,
            display: None,
            viewport_width: None,
            viewport_height: None,
        }
    }

    #[cfg(unix)]
    fn browser_params(thread_id: &str, tool: &str, arguments: Value) -> DynamicToolCallParams {
        DynamicToolCallParams {
            thread_id: thread_id.to_string(),
            turn_id: format!("{thread_id}-turn"),
            call_id: format!("{thread_id}-{tool}"),
            namespace: Some("codex_browser".to_string()),
            tool: tool.to_string(),
            arguments,
        }
    }

    #[cfg(unix)]
    async fn wait_for_fixture_file(path: &Path) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !path.exists() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("synthetic provider reached requested lifecycle seam");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn playwright_provider_keeps_thread_state_and_returns_typed_image() {
        let temp = tempfile::tempdir().expect("fixture root");
        let state_dir = temp.path().join("browser-state");
        let config = fake_provider_config(temp.path(), &state_dir);
        let manager = BrowserSessionManager::default();

        async fn call(
            manager: &BrowserSessionManager,
            config: &PlaywrightProviderConfig,
            thread_id: &str,
            tool: &str,
            arguments: Value,
        ) -> DynamicToolCallResponse {
            let params = browser_params(thread_id, tool, arguments);
            manager
                .handle(&params, config)
                .await
                .expect("synthetic Browser provider call")
        }

        let first_url = "https://same-thread.example/remembered";
        let first = call(
            &manager,
            &config,
            "thread-a",
            TOOL_BROWSER_OBSERVE,
            json!({"url": first_url}),
        )
        .await;
        assert!(first.success);
        assert!(response_includes_native_image(&first));

        let typed = call(
            &manager,
            &config,
            "thread-a",
            TOOL_BROWSER_STEP,
            json!({"action":"type","selector":"#note","text":"unsaved draft"}),
        )
        .await;
        assert!(typed.success);

        let continued = call(
            &manager,
            &config,
            "thread-a",
            TOOL_BROWSER_OBSERVE,
            json!({}),
        )
        .await;
        assert!(continued.success);
        assert!(response_includes_native_image(&continued));
        let text = continued.content_items.iter().find_map(|item| match item {
            DynamicToolCallOutputContentItem::InputText { text } => Some(text.as_str()),
            _ => None,
        });
        assert!(text.is_some_and(|text| text.contains(first_url)));
        assert!(text.is_some_and(|text| text.contains("unsaved draft")));

        let isolated = call(
            &manager,
            &config,
            "thread-b",
            TOOL_BROWSER_OBSERVE,
            json!({}),
        )
        .await;
        assert!(isolated.success);
        assert!(response_includes_native_image(&isolated));
        let text = isolated.content_items.iter().find_map(|item| match item {
            DynamicToolCallOutputContentItem::InputText { text } => Some(text.as_str()),
            _ => None,
        });
        assert!(text.is_some_and(|text| text.contains("about:blank")));

        let profiles = std::fs::read_dir(state_dir.join("profiles"))
            .expect("per-thread Browser profiles")
            .collect::<Result<Vec<_>, _>>()
            .expect("profile entries");
        assert_eq!(profiles.len(), 2);
        manager.shutdown_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_browser_action_closes_before_same_profile_retry() {
        let temp = tempfile::tempdir().expect("fixture root");
        let state_dir = temp.path().join("browser-state");
        std::fs::create_dir_all(&state_dir).expect("browser state root");
        std::fs::write(state_dir.join("pause-action"), "pause").expect("pause marker");
        let config = fake_provider_config(temp.path(), &state_dir);
        let manager = std::sync::Arc::new(BrowserSessionManager::default());
        let params = browser_params(
            "thread-cancel-action",
            TOOL_BROWSER_STEP,
            json!({"action":"type","selector":"#note","text":"uncertain"}),
        );
        let task_manager = std::sync::Arc::clone(&manager);
        let task_config = config.clone();
        let task = tokio::spawn(async move { task_manager.handle(&params, &task_config).await });
        wait_for_fixture_file(&state_dir.join("action-started")).await;
        task.abort();
        let _ = task.await;

        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            manager.handle(
                &browser_params("thread-cancel-action", TOOL_BROWSER_OBSERVE, json!({})),
                &config,
            ),
        )
        .await
        .expect("confirmed cleanup permits a same-profile retry")
        .expect("same-profile retry succeeds after cancellation cleanup");
        assert!(retry.success);
        assert!(state_dir.join("context-closed").exists());
        manager.shutdown_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closing_thread_cancels_in_flight_browser_action_without_replay() {
        let temp = tempfile::tempdir().expect("fixture root");
        let state_dir = temp.path().join("browser-state");
        std::fs::create_dir_all(&state_dir).expect("browser state root");
        std::fs::write(state_dir.join("pause-action"), "pause").expect("pause marker");
        let config = fake_provider_config(temp.path(), &state_dir);
        let manager = std::sync::Arc::new(BrowserSessionManager::default());
        let params = browser_params(
            "thread-close-action",
            TOOL_BROWSER_STEP,
            json!({"action":"type","selector":"#note","text":"uncertain"}),
        );
        let task_manager = std::sync::Arc::clone(&manager);
        let task_config = config.clone();
        let task = tokio::spawn(async move { task_manager.handle(&params, &task_config).await });
        wait_for_fixture_file(&state_dir.join("action-started")).await;

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            manager.close_thread("thread-close-action"),
        )
        .await
        .expect("thread close does not wait behind an active provider request");
        assert!(task.await.expect("request task exits").is_err());

        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            manager.handle(
                &browser_params("thread-close-action", TOOL_BROWSER_OBSERVE, json!({})),
                &config,
            ),
        )
        .await
        .expect("thread close cleanup remains bounded")
        .expect("confirmed cleanup permits a resumed same-profile call");
        assert!(retry.success);
        assert!(state_dir.join("context-closed").exists());
        manager.shutdown_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_pending_launch_closes_late_context_before_retry() {
        let temp = tempfile::tempdir().expect("fixture root");
        let state_dir = temp.path().join("browser-state");
        std::fs::create_dir_all(&state_dir).expect("browser state root");
        std::fs::write(state_dir.join("pause-launch"), "pause").expect("pause marker");
        let config = fake_provider_config(temp.path(), &state_dir);
        let manager = std::sync::Arc::new(BrowserSessionManager::default());
        let params = browser_params("thread-cancel-launch", TOOL_BROWSER_OBSERVE, json!({}));
        let task_manager = std::sync::Arc::clone(&manager);
        let task_config = config.clone();
        let task = tokio::spawn(async move { task_manager.handle(&params, &task_config).await });
        wait_for_fixture_file(&state_dir.join("launch-started")).await;
        task.abort();
        let _ = task.await;

        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            manager.handle(
                &browser_params("thread-cancel-launch", TOOL_BROWSER_OBSERVE, json!({})),
                &config,
            ),
        )
        .await
        .expect("late launch cleanup is bounded")
        .expect("confirmed cleanup permits a same-profile retry");
        assert!(retry.success);
        assert!(state_dir.join("context-closed").exists());
        manager.shutdown_all().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unconfirmed_browser_cleanup_retains_lock_and_rejects_retry() {
        let temp = tempfile::tempdir().expect("fixture root");
        let state_dir = temp.path().join("browser-state");
        std::fs::create_dir_all(&state_dir).expect("browser state root");
        std::fs::write(state_dir.join("pause-action"), "pause").expect("pause marker");
        std::fs::write(state_dir.join("hang-close"), "hang").expect("cleanup marker");
        let config = fake_provider_config(temp.path(), &state_dir);
        let manager = std::sync::Arc::new(BrowserSessionManager::default());
        let params = browser_params(
            "thread-unconfirmed-cleanup",
            TOOL_BROWSER_STEP,
            json!({"action":"type","selector":"#note","text":"uncertain"}),
        );
        let task_manager = std::sync::Arc::clone(&manager);
        let task_config = config.clone();
        let task = tokio::spawn(async move { task_manager.handle(&params, &task_config).await });
        wait_for_fixture_file(&state_dir.join("action-started")).await;
        task.abort();
        let _ = task.await;

        let retry = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            manager.handle(
                &browser_params(
                    "thread-unconfirmed-cleanup",
                    TOOL_BROWSER_OBSERVE,
                    json!({}),
                ),
                &config,
            ),
        )
        .await
        .expect("cleanup deadline is finite");
        assert!(retry.is_err());
        let profile = std::fs::read_dir(state_dir.join("profiles"))
            .expect("profile directory")
            .next()
            .expect("one thread profile")
            .expect("profile entry")
            .path();
        assert!(profile.join(".codex-provider.lock").is_dir());
        manager.shutdown_all().await;
    }
}
