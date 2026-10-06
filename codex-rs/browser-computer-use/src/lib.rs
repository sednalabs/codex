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
use std::env;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::time::timeout;

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
const ENV_PLAYWRIGHT_CAPTURE_MODE: &str = "CODEX_BROWSER_PLAYWRIGHT_CAPTURE_MODE";
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

/// Whether a dynamic tool name is one of the canonical browser tools.
pub fn is_browser_dynamic_tool(tool: &str) -> bool {
    matches!(tool, TOOL_BROWSER_OBSERVE | TOOL_BROWSER_STEP)
}

/// Return browser computer-use dynamic tools for the process default Codex home.
pub fn configured_browser_dynamic_tools() -> Vec<DynamicToolSpec> {
    let Some(codex_home) = default_codex_home() else {
        return Vec::new();
    };

    configured_browser_dynamic_tools_for_codex_home(codex_home.as_path())
}

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
                "Perform bounded browser actions, then return a fresh browser screenshot.",
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
                "scope": {
                    "type": "string",
                    "enum": ["viewport", "viewport_and_page"],
                    "description": "Optionally include a bounded page and control summary with actionable selector hints; form and editable values are never included."
                },
                "interaction_map": {
                    "type": "object",
                    "description": "Request a bounded, redacted page-control map with actionable selector hints.",
                    "properties": {
                        "scope": {"type": "string", "enum": ["page"], "description": "Include visible page controls only."},
                        "offset": {"type": "integer", "minimum": 0, "description": "Start position in the bounded control list."}
                    }
                },
                "captures": {
                    "type": "array",
                    "maxItems": 4,
                    "description": "Optionally request up to four labeled viewport captures. Each produces matching metadata and one inline PNG image; the original viewport and scroll are restored afterward.",
                    "items": {
                        "type": "object",
                        "required": ["label"],
                        "properties": {
                            "label": {"type": "string", "minLength": 1, "description": "Unique label paired with this capture's metadata and image."},
                            "viewportWidth": {"type": "integer", "minimum": 1, "description": "Requested capture viewport width in CSS pixels."},
                            "viewportHeight": {"type": "integer", "minimum": 1, "description": "Requested capture viewport height in CSS pixels."},
                            "scroll": {"type": "string", "enum": ["current", "top", "bottom"], "description": "Capture at the current scroll position, page top, or page bottom."},
                            "scrollY": {"type": "number", "minimum": 0, "description": "Capture at this vertical scroll position in CSS pixels."},
                            "settle_ms": {"type": "integer", "minimum": 0, "maximum": 2000, "description": "Bounded delay after applying capture viewport and scroll."}
                        },
                        "additionalProperties": false
                    }
                },
                "save_artifact": {
                    "type": "boolean",
                    "description": "When true, save capture PNGs and a paired manifest in a new private directory inside this thread's validated Browser profile. No caller-selected output path is accepted."
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

/// Handle a browser computer-use request using the process default Codex home.
pub async fn handle_browser_computer_use(
    params: &DynamicToolCallParams,
) -> BrowserComputerUseOutcome {
    let Some(codex_home) = default_codex_home() else {
        return BrowserComputerUseOutcome::Unavailable;
    };

    handle_browser_computer_use_for_codex_home(params, codex_home.as_path()).await
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

fn default_codex_home() -> Option<PathBuf> {
    codex_utils_home_dir::find_codex_home().ok().map(Into::into)
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
    let output = match run_provider_process(&command.argv, params, &[]).await {
        Ok(output) => output,
        Err(error) => {
            record_browser_provider_stage(&params.call_id, false, false, 0);
            return Err(error);
        }
    };
    match parse_provider_response(&output) {
        Ok(response) => {
            record_browser_provider_stage(
                &params.call_id,
                true,
                true,
                response.content_items.len(),
            );
            Ok(response)
        }
        Err(error) => {
            record_browser_provider_stage(&params.call_id, true, false, 0);
            Err(error)
        }
    }
}

fn record_browser_provider_stage(
    call_id: &str,
    process_exit_success: bool,
    json_parse_success: bool,
    content_item_count: usize,
) {
    if env::var("CODEX_TEST_BROWSER_OUTPUT_DIAGNOSTIC").ok().as_deref() != Some("1") {
        return;
    }
    let (Ok(expected_call_id), Some(codex_home)) = (
        env::var("CODEX_TEST_BROWSER_OUTPUT_DIAGNOSTIC_CALL_ID"),
        env::var_os("CODEX_HOME"),
    ) else {
        return;
    };
    let path = PathBuf::from(codex_home).join("browser-output-stage-diagnostic.jsonl");
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let observation = json!({
        "stage": "browser_provider",
        "call_id_matches_fixture": call_id == expected_call_id.as_str(),
        "provider_process_exit_success": process_exit_success,
        "provider_json_parse_success": json_parse_success,
        "provider_content_item_count": content_item_count,
    });
    if let Ok(mut line) = serde_json::to_vec(&observation) {
        line.push(b'\n');
        let _ = file.write_all(&line);
    }
}

async fn run_playwright_provider(
    params: &DynamicToolCallParams,
    config: &PlaywrightProviderConfig,
) -> Result<DynamicToolCallResponse, String> {
    let mut script_file = tempfile::Builder::new()
        .prefix("codex-browser-provider-")
        .suffix(".mjs")
        .tempfile()
        .map_err(|err| format!("failed to create browser provider script: {err}"))?;
    script_file
        .as_file_mut()
        .write_all(PLAYWRIGHT_BRIDGE_SCRIPT.as_bytes())
        .map_err(|err| format!("failed to write browser provider script: {err}"))?;

    let script_path = script_file.path().to_string_lossy().to_string();
    let envs = playwright_provider_envs(config);

    let output = run_provider_process(&[config.node.clone(), script_path], params, &envs).await?;
    parse_provider_response(&output)
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
        ENV_PLAYWRIGHT_CAPTURE_MODE,
        config.capture_mode.clone(),
    );
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
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "Browser provider exited with status {}: {}",
            output.status,
            compact_process_output(&stderr)
        ))
    }
}

fn parse_provider_response(bytes: &[u8]) -> Result<DynamicToolCallResponse, String> {
    serde_json::from_slice(bytes).map_err(|err| {
        let snippet = compact_process_output(&String::from_utf8_lossy(bytes));
        format!("failed to parse browser provider response: {err}; stdout: {snippet}")
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
                        capture_mode: env.capture_mode.clone().or_else(|| {
                            file.as_ref().and_then(|config| config.capture_mode.clone())
                        }),
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
    capture_mode: Option<String>,
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
    capture_mode: Option<String>,
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
    capture_mode: Option<String>,
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
                    capture_mode: self
                        .capture_mode
                        .clone()
                        .or_else(|| env.capture_mode.clone()),
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
    capture_mode: Option<String>,
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
            capture_mode: first_env(&[ENV_PLAYWRIGHT_CAPTURE_MODE]),
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
            "\n\n{missing_image_message} The browser provider must return screenshots as native image content items rather than text-only summaries or artifact paths."
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

fn compact_process_output(output: &str) -> String {
    const LIMIT: usize = 500;
    let compact = output.trim();
    if compact.chars().count() <= LIMIT {
        return compact.to_string();
    }
    let mut truncated = compact.chars().take(LIMIT - 3).collect::<String>();
    truncated.push_str("...");
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn command_spec_accepts_shell_like_string_and_array() {
        assert_eq!(
            command_spec_to_argv(CommandSpec::String("node provider.mjs".to_string())),
            Some(vec!["node".to_string(), "provider.mjs".to_string()])
        );
        assert_eq!(
            command_spec_to_argv(CommandSpec::Array(vec![
                "node".to_string(),
                "provider.mjs".to_string()
            ])),
            Some(vec!["node".to_string(), "provider.mjs".to_string()])
        );
    }

    #[test]
    fn requested_backend_defaults_to_auto() {
        assert_eq!(requested_backend(&json!({})), BACKEND_AUTO);
        assert_eq!(requested_backend(&json!({"backend": "iab"})), "iab");
        assert_eq!(requested_backend(&json!({"backend": "chrome"})), "chrome");
    }

    #[test]
    fn providers_array_routes_by_requested_backend() {
        let config = BrowserRuntimeConfig::from_sources(
            Some(BrowserRuntimeConfigFile {
                provider: None,
                command: None,
                node: None,
                node_path: None,
                timeout_secs: None,
                state_dir: None,
                headless: None,
                executable_path: None,
                channel: None,
                display: None,
                capture_mode: None,
                viewport_width: None,
                viewport_height: None,
                providers: Some(vec![
                    BrowserProviderConfigFile {
                        id: Some("chrome-provider".to_string()),
                        provider: Some(PROVIDER_COMMAND.to_string()),
                        command: Some(CommandSpec::Array(vec![
                            "node".to_string(),
                            "chrome-provider.mjs".to_string(),
                        ])),
                        node: None,
                        node_path: None,
                        timeout_secs: None,
                        state_dir: None,
                        headless: None,
                        executable_path: None,
                        channel: None,
                        display: None,
                        capture_mode: None,
                        viewport_width: None,
                        viewport_height: None,
                        backends: Some(vec!["chrome".to_string()]),
                        platforms: None,
                    },
                    BrowserProviderConfigFile {
                        id: Some("playwright".to_string()),
                        provider: Some(PROVIDER_PLAYWRIGHT.to_string()),
                        command: None,
                        node: Some("node".to_string()),
                        node_path: None,
                        timeout_secs: None,
                        state_dir: None,
                        headless: None,
                        executable_path: None,
                        channel: None,
                        display: None,
                        capture_mode: None,
                        viewport_width: None,
                        viewport_height: None,
                        backends: Some(vec![BACKEND_AUTO.to_string()]),
                        platforms: None,
                    },
                ]),
                routing: None,
            }),
            BrowserRuntimeEnv::default(),
        )
        .expect("configured providers");

        assert_eq!(
            config.provider_for_backend("chrome").map(|p| &p.id),
            Some(&"chrome-provider".to_string())
        );
        assert_eq!(
            config.provider_for_backend(BACKEND_AUTO).map(|p| &p.id),
            Some(&"playwright".to_string())
        );
        assert_eq!(config.provider_for_backend("iab"), None);
    }

    #[test]
    fn routing_fallback_order_controls_wildcard_provider_preference() {
        let config = BrowserRuntimeConfig::from_sources(
            Some(BrowserRuntimeConfigFile {
                provider: None,
                command: None,
                node: None,
                node_path: None,
                timeout_secs: None,
                state_dir: None,
                headless: None,
                executable_path: None,
                channel: None,
                display: None,
                capture_mode: None,
                viewport_width: None,
                viewport_height: None,
                providers: Some(vec![
                    BrowserProviderConfigFile {
                        id: Some("first".to_string()),
                        provider: Some(PROVIDER_COMMAND.to_string()),
                        command: Some(CommandSpec::Array(vec!["first".to_string()])),
                        node: None,
                        node_path: None,
                        timeout_secs: None,
                        state_dir: None,
                        headless: None,
                        executable_path: None,
                        channel: None,
                        display: None,
                        capture_mode: None,
                        viewport_width: None,
                        viewport_height: None,
                        backends: None,
                        platforms: None,
                    },
                    BrowserProviderConfigFile {
                        id: Some("second".to_string()),
                        provider: Some(PROVIDER_COMMAND.to_string()),
                        command: Some(CommandSpec::Array(vec!["second".to_string()])),
                        node: None,
                        node_path: None,
                        timeout_secs: None,
                        state_dir: None,
                        headless: None,
                        executable_path: None,
                        channel: None,
                        display: None,
                        capture_mode: None,
                        viewport_width: None,
                        viewport_height: None,
                        backends: None,
                        platforms: None,
                    },
                ]),
                routing: Some(BrowserRoutingConfigFile {
                    fallback_order: Some(vec!["second".to_string(), "first".to_string()]),
                }),
            }),
            BrowserRuntimeEnv::default(),
        )
        .expect("configured providers");

        assert_eq!(
            config.provider_for_backend(BACKEND_AUTO).map(|p| &p.id),
            Some(&"second".to_string())
        );
    }

    #[test]
    fn legacy_playwright_provider_claims_chrome_browser_backend_aliases() {
        let config = BrowserRuntimeConfig::from_sources(
            Some(BrowserRuntimeConfigFile {
                provider: Some(PROVIDER_PLAYWRIGHT.to_string()),
                command: None,
                node: Some("node".to_string()),
                node_path: None,
                timeout_secs: None,
                state_dir: None,
                headless: None,
                executable_path: None,
                channel: None,
                display: None,
                capture_mode: None,
                viewport_width: None,
                viewport_height: None,
                providers: None,
                routing: None,
            }),
            BrowserRuntimeEnv::default(),
        )
        .expect("playwright provider");

        assert!(config.provider_for_backend(BACKEND_AUTO).is_some());
        assert!(config.provider_for_backend(BACKEND_BROWSER).is_some());
        assert!(config.provider_for_backend(BACKEND_CHROME).is_some());
        assert!(config.provider_for_backend(BACKEND_CHROMIUM).is_some());
        assert!(config.provider_for_backend("iab").is_none());
    }

    #[test]
    fn playwright_provider_exports_real_browser_environment() {
        let config = PlaywrightProviderConfig {
            node: "node".to_string(),
            node_path: Some("/opt/node_modules".to_string()),
            state_dir: Some("/tmp/codex-browser".to_string()),
            headless: Some(false),
            executable_path: Some("/usr/bin/google-chrome".to_string()),
            channel: None,
            display: Some(":99".to_string()),
            capture_mode: Some("viewport".to_string()),
            viewport_width: Some(1440),
            viewport_height: Some(1000),
        };

        assert_eq!(
            playwright_provider_envs(&config),
            vec![
                (
                    ENV_PLAYWRIGHT_NODE_PATH.to_string(),
                    "/opt/node_modules".to_string()
                ),
                ("NODE_PATH".to_string(), "/opt/node_modules".to_string()),
                (
                    ENV_PLAYWRIGHT_STATE_DIR.to_string(),
                    "/tmp/codex-browser".to_string()
                ),
                (
                    ENV_PLAYWRIGHT_EXECUTABLE_PATH.to_string(),
                    "/usr/bin/google-chrome".to_string()
                ),
                (ENV_PLAYWRIGHT_DISPLAY.to_string(), ":99".to_string()),
                ("DISPLAY".to_string(), ":99".to_string()),
                (
                    ENV_PLAYWRIGHT_CAPTURE_MODE.to_string(),
                    "viewport".to_string()
                ),
                (
                    ENV_PLAYWRIGHT_VIEWPORT_WIDTH.to_string(),
                    "1440".to_string()
                ),
                (
                    ENV_PLAYWRIGHT_VIEWPORT_HEIGHT.to_string(),
                    "1000".to_string()
                ),
                (ENV_PLAYWRIGHT_HEADLESS.to_string(), "0".to_string()),
            ]
        );
    }

    #[test]
    fn configured_browser_tools_are_session_scoped_native_tools() {
        let tools = [
            browser_dynamic_tool(TOOL_BROWSER_OBSERVE, "observe"),
            browser_dynamic_tool(TOOL_BROWSER_STEP, "step"),
        ];

        assert!(matches!(
            &tools[0],
            DynamicToolNamespaceTool::Function(spec) if spec.name == TOOL_BROWSER_OBSERVE && !spec.defer_loading
        ));
        assert!(matches!(
            &tools[1],
            DynamicToolNamespaceTool::Function(spec) if spec.name == TOOL_BROWSER_STEP && !spec.defer_loading
        ));
        let DynamicToolNamespaceTool::Function(spec) = &tools[0];
        let properties = spec.input_schema["properties"]
            .as_object()
            .expect("Browser visual request properties");
        for property in ["scope", "interaction_map", "captures", "save_artifact"] {
            assert!(
                properties.contains_key(property),
                "missing schema property {property}"
            );
            assert!(
                properties[property]["description"]
                    .as_str()
                    .is_some_and(|description| !description.trim().is_empty()),
                "schema property {property} needs a useful description"
            );
        }
        assert_eq!(properties["captures"]["maxItems"], 4);
        for property in [
            "label",
            "viewportWidth",
            "viewportHeight",
            "scroll",
            "scrollY",
            "settle_ms",
        ] {
            assert!(
                properties["captures"]["items"]["properties"]
                    .get(property)
                    .is_some()
            );
        }
    }

    #[test]
    fn configured_browser_tools_load_from_explicit_codex_home() {
        let codex_home = tempfile::tempdir().expect("temp codex home");
        std::fs::write(
            codex_home.path().join("browser-computer-use.json"),
            r#"{"provider":"playwright"}"#,
        )
        .expect("write browser provider config");

        let tools = configured_browser_dynamic_tools_for_codex_home(codex_home.path());

        assert_eq!(
            tools
                .iter()
                .flat_map(|tool| match tool {
                    DynamicToolSpec::Namespace(namespace) => namespace
                        .tools
                        .iter()
                        .map(|tool| match tool {
                            DynamicToolNamespaceTool::Function(spec) => spec.name.as_str(),
                        })
                        .collect::<Vec<_>>(),
                    DynamicToolSpec::Function(_) => Vec::new(),
                })
                .collect::<Vec<_>>(),
            vec![TOOL_BROWSER_OBSERVE, TOOL_BROWSER_STEP]
        );
    }

    #[test]
    fn browser_provider_response_preserves_native_image() {
        let mut response = DynamicToolCallResponse {
            content_items: vec![
                DynamicToolCallOutputContentItem::InputText {
                    text: "Browser observation".to_string(),
                },
                DynamicToolCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAAA".to_string(),
                },
            ],
            success: true,
        };

        require_native_image_for_visual_response(
            &mut response,
            "Browser observation missing native image output.",
        );

        assert!(response.success);
    }

    #[test]
    fn browser_provider_response_without_native_image_fails_loudly() {
        let mut response = DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: "Browser observation\nurl: https://example.test".to_string(),
            }],
            success: true,
        };

        require_native_image_for_visual_response(
            &mut response,
            "Browser observation missing native image output.",
        );

        assert!(!response.success);
        let DynamicToolCallOutputContentItem::InputText { text } = &response.content_items[0]
        else {
            panic!("expected text summary");
        };
        assert!(text.contains("url: https://example.test"));
        assert!(text.contains("Browser observation missing native image output."));
        assert!(text.contains("must return screenshots as native image content items"));
    }

    #[test]
    fn missing_native_image_diagnostic_is_visible_for_empty_response() {
        let mut response = DynamicToolCallResponse {
            content_items: vec![],
            success: true,
        };

        require_native_image_for_visual_response(
            &mut response,
            "Browser observation missing native image output.",
        );

        assert!(!response.success);
        assert_eq!(response.content_items.len(), 1);
        let DynamicToolCallOutputContentItem::InputText { text } = &response.content_items[0]
        else {
            panic!("expected text diagnostic");
        };
        assert!(text.contains("Browser observation missing native image output."));
        assert!(text.contains("must return screenshots as native image content items"));
    }

    #[test]
    fn parse_provider_response_accepts_computer_use_content_items() {
        let response = parse_provider_response(
            br#"{
              "contentItems": [
                {"type":"inputText","text":"Browser observation"},
                {"type":"inputImage","imageUrl":"data:image/png;base64,AAAA","detail":"high"}
              ],
              "success": true
            }"#,
        )
        .expect("valid provider response");

        assert_eq!(
            response.content_items,
            vec![
                DynamicToolCallOutputContentItem::InputText {
                    text: "Browser observation".to_string(),
                },
                DynamicToolCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAAA".to_string(),
                },
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_provider_bridge_returns_native_image_response() {
        let mut provider = tempfile::NamedTempFile::new().expect("temp provider");
        provider
            .write_all(
                br#"#!/bin/sh
cat >/dev/null
cat <<'JSON'
{"contentItems":[{"type":"inputText","text":"Browser observation from fake provider"},{"type":"inputImage","imageUrl":"data:image/png;base64,AAAA","detail":"high"}],"success":true}
JSON
"#,
            )
            .expect("write provider");
        let params = DynamicToolCallParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            namespace: Some("codex_browser".to_string()),
            tool: TOOL_BROWSER_OBSERVE.to_string(),
            arguments: json!({}),
        };

        let response = run_command_provider(
            &params,
            &CommandProviderConfig {
                argv: vec![
                    "sh".to_string(),
                    provider.path().to_string_lossy().to_string(),
                ],
            },
        )
        .await
        .expect("provider response");

        assert!(response.success);
        assert!(response_includes_native_image(&response));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn playwright_provider_preserves_state_roots_and_thread_profiles() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::path::Component;

        let temp = tempfile::tempdir().expect("temporary fixture root");
        let codex_home = temp.path().join("codex-home");
        let node_path = temp.path().join("node-modules");
        std::fs::create_dir(&codex_home).expect("Codex home");
        std::fs::create_dir_all(node_path.join("playwright")).expect("fake Playwright module");
        std::fs::write(
            node_path.join("playwright/index.js"),
            r#"
let currentUrl = "about:blank";
let screenshotCount = 0;
const browserState = { width: 1280, height: 720, scrollX: 0, scrollY: 0, devicePixelRatio: 2 };
global.window = {};
Object.defineProperties(global.window, {
  innerWidth: { get: () => browserState.width },
  innerHeight: { get: () => browserState.height },
  devicePixelRatio: { get: () => browserState.devicePixelRatio },
  scrollX: { get: () => browserState.scrollX },
  scrollY: { get: () => browserState.scrollY },
});
global.window.scrollTo = (x, y) => { browserState.scrollX = x; browserState.scrollY = Math.max(0, Math.min(y, 1080)); };
global.document = { documentElement: {
  get clientWidth() { return browserState.width - (process.env.CODEX_BROWSER_FIXTURE_MISMATCH_RESTORE === "1" && screenshotCount > 0 && browserState.width === 1280 ? 1 : 0); },
  get clientHeight() { return browserState.height; },
  scrollWidth: 1280,
  scrollHeight: 1800,
}, querySelectorAll: () => [
  {
    tagName: "DIV", isContentEditable: false, hidden: false, disabled: false,
    innerText: "DO_NOT_EXPOSE_ARIA_TEXTBOX_SECRET", textContent: "DO_NOT_EXPOSE_ARIA_TEXTBOX_SECRET",
    getAttribute: (name) => name === "role" ? "textbox" : null,
    getBoundingClientRect: () => ({ x: 10, y: 20, width: 180, height: 30 }),
  },
  {
    tagName: "DIV", isContentEditable: false, hidden: false, disabled: false,
    innerText: "DO_NOT_EXPOSE_ARIA_COMBOBOX_SECRET", textContent: "DO_NOT_EXPOSE_ARIA_COMBOBOX_SECRET",
    getAttribute: (name) => name === "role" ? "combobox" : null,
    getBoundingClientRect: () => ({ x: 10, y: 60, width: 180, height: 30 }),
  },
  {
    tagName: "BUTTON", isContentEditable: false, hidden: false, disabled: false,
    innerText: "Save draft", textContent: "Save draft",
    getAttribute: (name) => name === "aria-label" ? 'Save "draft" \\ now' : null,
    getBoundingClientRect: () => ({ x: 10, y: 100, width: 120, height: 30 }),
  },
] };
const page = {
  isClosed: () => false,
  url: () => currentUrl,
  goto: async (url) => { currentUrl = url; },
  waitForLoadState: async () => {},
  waitForTimeout: async () => {},
  screenshot: async () => {
    screenshotCount += 1;
    if (process.env.CODEX_BROWSER_FIXTURE_FAIL_SCREENSHOT === "1" || (process.env.CODEX_BROWSER_FIXTURE_FAIL_SCREENSHOT_AFTER_FIRST === "1" && screenshotCount === 2)) throw new Error("fixture screenshot failure");
    const signature = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    return Buffer.concat([signature, Buffer.from(`fixture-image-${screenshotCount}-${browserState.scrollY}`)]);
  },
  setViewportSize: async ({ width, height }) => {
    browserState.width = width;
    browserState.height = height;
    if (process.env.CODEX_BROWSER_FIXTURE_MISMATCH_RESTORE === "1" && screenshotCount > 0 && width === 1280) browserState.devicePixelRatio = 1.5;
  },
  evaluate: async (fn, arg) => {
    if (process.env.CODEX_BROWSER_FIXTURE_FAIL_RESTORE === "1" && fn.toString().includes("scroll.x")) {
      throw new Error("fixture restoration failure");
    }
    return fn(arg);
  },
  title: async () => "fixture page",
  viewportSize: () => ({ width: browserState.width, height: browserState.height }),
};
exports.chromium = {
  launchPersistentContext: async (stateDir) => {
    require("node:fs").mkdirSync(stateDir, { recursive: true });
    return {
      pages: () => [],
      newPage: async () => page,
      close: async () => {},
    };
  },
};
"#,
        )
        .expect("fake Playwright module source");

        let make_node_wrapper = |unset_state_dir: bool, fixture_env: &[(&str, &str)]| {
            let mut node = tempfile::NamedTempFile::new().expect("Node wrapper");
            let unset_state_dir = if unset_state_dir {
                "unset CODEX_BROWSER_PLAYWRIGHT_STATE_DIR\n"
            } else {
                ""
            };
            let fixture_env = fixture_env
                .iter()
                .map(|(name, value)| format!("export {name}='{value}'\n"))
                .collect::<String>();
            let script = format!(
                "#!/bin/sh\n{unset_state_dir}unset CODEX_BROWSER_PLAYWRIGHT_ISOLATION\nexport CODEX_HOME='{}'\n{fixture_env}exec node \"$@\"\n",
                codex_home.display(),
            );
            node.write_all(script.as_bytes())
                .expect("write Node wrapper");
            std::fs::set_permissions(node.path(), std::fs::Permissions::from_mode(0o700))
                .expect("make Node wrapper executable");
            // Closing the writable handle is required before Linux can execute the wrapper.
            let node = node.into_temp_path();
            let node_path: std::path::PathBuf =
                <tempfile::TempPath as std::convert::AsRef<std::path::Path>>::as_ref(&node)
                    .to_path_buf();
            (node, node_path)
        };
        let (_configured_node, configured_node_path) = make_node_wrapper(false, &[]);
        let (_default_node, default_node_path) = make_node_wrapper(true, &[]);

        let run = |state_dir: Option<String>, thread_id: &str| {
            let node_path = node_path.to_string_lossy().to_string();
            let node = if state_dir.is_some() {
                configured_node_path.to_string_lossy().to_string()
            } else {
                default_node_path.to_string_lossy().to_string()
            };
            let thread_id = thread_id.to_string();
            async move {
                let config = PlaywrightProviderConfig {
                    node,
                    node_path: Some(node_path),
                    state_dir,
                    headless: Some(true),
                    executable_path: None,
                    channel: None,
                    display: None,
                    capture_mode: None,
                    viewport_width: None,
                    viewport_height: None,
                };
                let params = DynamicToolCallParams {
                    thread_id,
                    turn_id: "turn-fixture".to_string(),
                    call_id: "call-fixture".to_string(),
                    namespace: Some("codex_browser".to_string()),
                    tool: TOOL_BROWSER_OBSERVE.to_string(),
                    arguments: json!({}),
                };
                run_playwright_provider(&params, &config).await
            }
        };
        let run_visual = |state_dir: PathBuf, thread_id: &str, arguments: Value, node: PathBuf| {
            let node_path = node_path.to_string_lossy().to_string();
            let node = node.to_string_lossy().to_string();
            let state_dir = state_dir.to_string_lossy().to_string();
            let thread_id = thread_id.to_string();
            async move {
                let config = PlaywrightProviderConfig {
                    node,
                    node_path: Some(node_path),
                    state_dir: Some(state_dir),
                    headless: Some(true),
                    executable_path: None,
                    channel: None,
                    display: None,
                    capture_mode: None,
                    viewport_width: None,
                    viewport_height: None,
                };
                let params = DynamicToolCallParams {
                    thread_id,
                    turn_id: "visual-fixture-turn".to_string(),
                    call_id: "visual-fixture-call".to_string(),
                    namespace: Some("codex_browser".to_string()),
                    tool: TOOL_BROWSER_OBSERVE.to_string(),
                    arguments,
                };
                run_playwright_provider(&params, &config).await
            }
        };

        fn decode_inline_png(image_url: &str) -> Vec<u8> {
            const ALPHABET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let encoded = image_url
                .strip_prefix("data:image/png;base64,")
                .expect("typed inline PNG data URL");
            let mut decoded = Vec::new();
            let mut accumulator = 0u32;
            let mut bits = 0u8;
            for byte in encoded.bytes().take_while(|byte| *byte != b'=') {
                let value = ALPHABET
                    .iter()
                    .position(|candidate| *candidate == byte)
                    .expect("valid base64 image data") as u32;
                accumulator = (accumulator << 6) | value;
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    decoded.push((accumulator >> bits) as u8);
                }
            }
            decoded
        }

        let absolute_root = temp.path().join("custom-state");
        let absolute = absolute_root.to_string_lossy().to_string();
        for thread_id in ["same-thread", "same-thread", "different-thread"] {
            let response = run(Some(absolute.clone()), thread_id)
                .await
                .expect("actual Rust-to-embedded-MJS provider call");
            assert!(response.success);
            assert!(response_includes_native_image(&response));
        }
        let absolute_profiles = std::fs::read_dir(absolute_root.join("profiles"))
            .expect("absolute-root profiles")
            .map(|entry| {
                entry
                    .expect("profile entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(absolute_profiles.len(), 2);
        assert!(
            absolute_profiles
                .iter()
                .any(|name| name == "same-thread-d2c13f2a0d21")
        );
        assert!(
            absolute_profiles
                .iter()
                .any(|name| name == "different-thread-a51f589e498c")
        );
        assert!(
            absolute_root
                .join("profiles/same-thread-d2c13f2a0d21/state.json")
                .is_file()
        );
        assert!(
            absolute_root
                .join("profiles/different-thread-a51f589e498c/state.json")
                .is_file()
        );

        let relative_root = temp.path().join("legacy-state");
        let provider_cwd = std::env::current_dir().expect("inherited provider cwd");
        let from = provider_cwd.components().collect::<Vec<_>>();
        let to = relative_root.components().collect::<Vec<_>>();
        let common = from
            .iter()
            .zip(&to)
            .take_while(|(left, right)| left == right)
            .count();
        let mut relative_state_dir = std::path::PathBuf::new();
        for component in from.iter().skip(common) {
            if matches!(component, Component::Normal(_)) {
                relative_state_dir.push("..");
            }
        }
        for component in to.iter().skip(common) {
            relative_state_dir.push(component.as_os_str());
        }
        let response = run(
            Some(relative_state_dir.to_string_lossy().to_string()),
            "relative-thread",
        )
        .await
        .expect("legacy relative root resolves against inherited provider cwd");
        assert!(response.success);
        assert!(response_includes_native_image(&response));
        assert_eq!(
            std::fs::read_dir(relative_root.join("profiles"))
                .expect("relative-root profiles")
                .count(),
            1
        );

        let default_root = codex_home.join("browser-computer-use-playwright");
        let response = run(None, "default-thread")
            .await
            .expect("default root follows active CODEX_HOME");
        assert!(response.success);
        assert!(response_includes_native_image(&response));
        assert!(default_root.join("profiles").is_dir());

        let symlink_target = temp.path().join("symlink-target");
        std::fs::create_dir(&symlink_target).expect("symlink target");
        let symlink_root = temp.path().join("symlink-state");
        std::os::unix::fs::symlink(&symlink_target, &symlink_root).expect("state-root symlink");
        let response = run(
            Some(symlink_root.to_string_lossy().to_string()),
            "symlink-thread",
        )
        .await
        .expect("provider reports unsafe state-root configuration");
        assert!(!response.success);
        let DynamicToolCallOutputContentItem::InputText { text } = &response.content_items[0]
        else {
            panic!("expected actionable state-path error");
        };
        assert!(text.contains("must not be a symlink"));

        let writable_parent = temp.path().join("writable-parent");
        std::fs::create_dir(&writable_parent).expect("writable parent");
        std::fs::set_permissions(&writable_parent, std::fs::Permissions::from_mode(0o777))
            .expect("make parent writable for other users");
        let response = run(
            Some(writable_parent.join("state").to_string_lossy().to_string()),
            "writable-parent-thread",
        )
        .await
        .expect("provider reports unsafe writable ancestor");
        assert!(!response.success);
        let DynamicToolCallOutputContentItem::InputText { text } = &response.content_items[0]
        else {
            panic!("expected actionable state-path error");
        };
        assert!(text.contains("writable by untrusted users"));

        let response = run(Some(absolute), "")
            .await
            .expect("provider rejects missing thread id without fallback");
        assert!(!response.success);
        let DynamicToolCallOutputContentItem::InputText { text } = &response.content_items[0]
        else {
            panic!("expected actionable thread-isolation error");
        };
        assert!(text.contains("requires a non-empty threadId"));

        let (_capture_failure_wrapper, capture_failure_node) = make_node_wrapper(
            false,
            &[("CODEX_BROWSER_FIXTURE_FAIL_SCREENSHOT_AFTER_FIRST", "1")],
        );
        let capture_failure_root = temp.path().join("capture-failure-state");
        let capture_failure = run_visual(
            capture_failure_root.clone(),
            "capture-failure-thread",
            json!({
                "captures": [
                    {"label": "captured-partial", "scroll": "top"},
                    {"label": "will-fail", "scroll": "bottom"}
                ],
                "save_artifact": true
            }),
            capture_failure_node,
        )
        .await
        .expect("capture failure response");
        assert!(!capture_failure.success);
        assert!(response_includes_native_image(&capture_failure));
        assert_eq!(
            capture_failure
                .content_items
                .iter()
                .filter(|item| matches!(item, DynamicToolCallOutputContentItem::InputImage { .. }))
                .count(),
            1,
            "successful first capture survives the second capture failure"
        );
        let DynamicToolCallOutputContentItem::InputText { text } =
            &capture_failure.content_items[0]
        else {
            panic!("capture failure includes stage text");
        };
        assert!(text.contains("visual_error: capture: Unable to capture browser screenshot."));
        assert!(text.contains("page.screenshot: fixture screenshot failure"));
        assert!(
            text.contains("\"success\":true"),
            "capture failure still restores state"
        );
        let capture_failure_profile = std::fs::read_dir(capture_failure_root.join("profiles"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(!capture_failure_profile.join("artifacts").exists());

        let (_restore_failure_wrapper, restore_failure_node) =
            make_node_wrapper(false, &[("CODEX_BROWSER_FIXTURE_FAIL_RESTORE", "1")]);
        let restore_failure_root = temp.path().join("restore-failure-state");
        let restore_failure = run_visual(
            restore_failure_root.clone(),
            "restore-failure-thread",
            json!({
                "captures": [{"label": "partial", "scroll": "bottom"}],
                "save_artifact": true
            }),
            restore_failure_node,
        )
        .await
        .expect("restoration failure response");
        assert!(!restore_failure.success);
        assert!(response_includes_native_image(&restore_failure));
        let DynamicToolCallOutputContentItem::InputText { text } =
            &restore_failure.content_items[0]
        else {
            panic!("restoration failure includes stage text");
        };
        assert!(text.contains("restoration: fixture restoration failure"));
        assert!(text.contains("\"success\":false"));
        let restore_failure_profile = std::fs::read_dir(restore_failure_root.join("profiles"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(!restore_failure_profile.join("artifacts").exists());

        let artifact_failure_root = temp.path().join("artifact-failure-state");
        let artifact_thread = "artifact-failure-thread";
        let initialized = run_visual(
            artifact_failure_root.clone(),
            artifact_thread,
            json!({}),
            configured_node_path.clone(),
        )
        .await
        .expect("initialize disposable artifact profile");
        assert!(initialized.success);
        let artifact_profile = std::fs::read_dir(artifact_failure_root.join("profiles"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let artifact_parent = artifact_profile.join("artifacts");
        std::fs::write(&artifact_parent, "not a directory")
            .expect("install deterministic artifact failure");
        let artifact_failure = run_visual(
            artifact_failure_root.clone(),
            artifact_thread,
            json!({
                "captures": [{"label": "captured-before-save-failure"}],
                "save_artifact": true
            }),
            configured_node_path.clone(),
        )
        .await
        .expect("artifact failure response");
        assert!(!artifact_failure.success);
        assert!(response_includes_native_image(&artifact_failure));
        let DynamicToolCallOutputContentItem::InputText { text } =
            &artifact_failure.content_items[0]
        else {
            panic!("artifact failure includes stage text");
        };
        assert!(text.contains("artifact_save:"));
        assert!(
            text.contains("\"success\":true"),
            "artifact failure keeps verified restoration status"
        );
        assert!(artifact_parent.is_file());

        let partial_artifact_root = temp.path().join("partial-artifact-state");
        let partial_artifact_thread = "partial-artifact-thread";
        let initialized = run_visual(
            partial_artifact_root.clone(),
            partial_artifact_thread,
            json!({}),
            configured_node_path.clone(),
        )
        .await
        .expect("initialize partial-artifact profile");
        assert!(initialized.success);
        let partial_artifact_profile = std::fs::read_dir(partial_artifact_root.join("profiles"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let mut write_failure_preload = tempfile::Builder::new()
            .suffix(".cjs")
            .tempfile()
            .expect("second PNG failure preload");
        write_failure_preload
            .write_all(
                br#"
const fs = require("node:fs/promises");
const writeFile = fs.writeFile.bind(fs);
fs.writeFile = async (file, ...args) => {
  if (String(file).endsWith("capture-02.png")) throw new Error("fixture second artifact PNG failure");
  return writeFile(file, ...args);
};
"#,
            )
            .expect("write second-PNG failure preload");
        let write_failure_preload_path = write_failure_preload.path().to_string_lossy().to_string();
        let node_options = format!("--require={write_failure_preload_path}");
        let (_partial_artifact_wrapper, partial_artifact_node) =
            make_node_wrapper(false, &[("NODE_OPTIONS", node_options.as_str())]);
        let partial_artifact_failure = run_visual(
            partial_artifact_root.clone(),
            partial_artifact_thread,
            json!({
                "captures": [{"label": "first"}, {"label": "second"}],
                "save_artifact": true
            }),
            partial_artifact_node,
        )
        .await
        .expect("partial artifact-write failure response");
        assert!(!partial_artifact_failure.success);
        assert_eq!(
            partial_artifact_failure
                .content_items
                .iter()
                .filter(|item| matches!(item, DynamicToolCallOutputContentItem::InputImage { .. }))
                .count(),
            2,
            "both completed captures remain available despite artifact write failure"
        );
        let DynamicToolCallOutputContentItem::InputText { text } =
            &partial_artifact_failure.content_items[0]
        else {
            panic!("partial artifact failure includes stage text");
        };
        assert!(text.contains("artifact_save: fixture second artifact PNG failure"));
        let partial: Value = serde_json::from_str(
            text.lines()
                .find_map(|line| line.strip_prefix("artifact_partial: "))
                .expect("partial artifact inventory is reported"),
        )
        .expect("valid partial artifact inventory");
        assert_eq!(partial["complete_manifest"], false);
        assert_eq!(partial["files"], json!(["capture-01.png"]));
        let partial_directory = partial["directory"]
            .as_str()
            .expect("private run directory");
        assert!(
            Path::new(partial_directory)
                .components()
                .all(|component| { matches!(component, std::path::Component::Normal(_)) })
        );
        let partial_run_dir = partial_artifact_profile.join(partial_directory);
        let retained_png = std::fs::read(partial_run_dir.join("capture-01.png"))
            .expect("first successfully written artifact remains available");
        assert!(!retained_png.is_empty());
        assert!(!partial_run_dir.join("capture-02.png").exists());
        assert!(!partial_run_dir.join("manifest.json").exists());

        let unsafe_artifact_root = temp.path().join("unsafe-artifact-state");
        let unsafe_thread = "unsafe-artifact-thread";
        let initialized = run_visual(
            unsafe_artifact_root.clone(),
            unsafe_thread,
            json!({}),
            configured_node_path.clone(),
        )
        .await
        .expect("initialize unsafe-artifact profile");
        assert!(initialized.success);
        let unsafe_profile = std::fs::read_dir(unsafe_artifact_root.join("profiles"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let outside_target = temp.path().join("artifact-symlink-target");
        std::fs::create_dir(&outside_target).expect("outside artifact symlink target");
        std::os::unix::fs::symlink(&outside_target, unsafe_profile.join("artifacts"))
            .expect("install unsafe artifact symlink");
        let unsafe_artifact = run_visual(
            unsafe_artifact_root.clone(),
            unsafe_thread,
            json!({"captures": [{"label": "unsafe-path"}], "save_artifact": true}),
            configured_node_path.clone(),
        )
        .await
        .expect("unsafe artifact path response");
        assert!(!unsafe_artifact.success);
        assert!(response_includes_native_image(&unsafe_artifact));
        let DynamicToolCallOutputContentItem::InputText { text } =
            &unsafe_artifact.content_items[0]
        else {
            panic!("unsafe artifact path includes stage text");
        };
        assert!(text.contains("artifact_save:"));
        assert!(text.contains("must not be a symlink"));
        assert_eq!(std::fs::read_dir(outside_target).unwrap().count(), 0);

        let aria_value_root = temp.path().join("aria-value-state");
        let aria_value_response = run_visual(
            aria_value_root,
            "aria-value-thread",
            json!({"scope": "viewport_and_page"}),
            configured_node_path.clone(),
        )
        .await
        .expect("custom ARIA value-redaction response");
        assert!(aria_value_response.success);
        let DynamicToolCallOutputContentItem::InputText { text } =
            &aria_value_response.content_items[0]
        else {
            panic!("page-control hints are text metadata");
        };
        assert!(text.contains("\"role\":\"textbox\""));
        assert!(text.contains("\"role\":\"combobox\""));
        assert!(!text.contains("DO_NOT_EXPOSE_ARIA_TEXTBOX_SECRET"));
        assert!(!text.contains("DO_NOT_EXPOSE_ARIA_COMBOBOX_SECRET"));

        let visual_fixture_root = temp.path().join("visual-pair-state");
        let visual_fixture = run_visual(
            visual_fixture_root.clone(),
            "visual-pair-thread",
            json!({
                "scope": "viewport_and_page",
                "captures": [
                    {"label": "top-wide", "viewportWidth": 960, "viewportHeight": 640, "scroll": "top"},
                    {"label": "bottom-narrow", "viewportWidth": 640, "viewportHeight": 480, "scroll": "bottom"}
                ],
                "save_artifact": true
            }),
            configured_node_path.clone(),
        )
        .await
        .expect("successful synthetic visual capture with saved artifacts");
        assert!(visual_fixture.success, "{visual_fixture:?}");
        let visual_fixture_text = visual_fixture
            .content_items
            .iter()
            .find_map(|item| match item {
                DynamicToolCallOutputContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("textual capture metadata");
        assert!(!visual_fixture_text.contains("data:image/png;base64,"));
        let hints_json = visual_fixture_text
            .lines()
            .find_map(|line| line.strip_prefix("page_hints: "))
            .expect("page/control hints are reported");
        let hints: Value = serde_json::from_str(hints_json).expect("valid page hint JSON");
        assert!(hints["controls"].as_array().unwrap().iter().any(|control| {
            control["selectors"].as_array().unwrap().iter().any(|selector| {
                selector.as_str() == Some(r#"[aria-label="Save \22 draft\22  \5c  now"]"#)
            })
        }), "quotes and backslashes in control labels are CSS-escaped");

        let inline_images = visual_fixture
            .content_items
            .iter()
            .filter_map(|item| match item {
                DynamicToolCallOutputContentItem::InputImage { image_url } => {
                    Some(decode_inline_png(image_url))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(inline_images.len(), 2, "one inline image per labeled capture");
        assert_ne!(inline_images[0], inline_images[1], "fixture captures are distinct");
        const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
        let profile = std::fs::read_dir(visual_fixture_root.join("profiles"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let manifest_relative_path = visual_fixture_text
            .lines()
            .find_map(|line| line.strip_prefix("artifact_manifest: "))
            .expect("saved manifest path is reported");
        let manifest_path = profile.join(manifest_relative_path);
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(&manifest_path).expect("saved capture manifest"),
        )
        .expect("valid saved capture manifest");
        let captures = manifest["captures"].as_array().expect("manifest captures");
        assert_eq!(captures.len(), 2);
        for (index, expected_label) in ["top-wide", "bottom-narrow"].iter().enumerate() {
            let capture = &captures[index];
            let expected_width = if index == 0 { 960 } else { 640 };
            assert_eq!(capture["order"], index + 1);
            assert_eq!(capture["label"], *expected_label);
            assert_eq!(capture["metadata"]["requestedViewport"]["width"], expected_width);
            assert_eq!(capture["metadata"]["effectiveViewport"]["width"], expected_width);
            assert_eq!(capture["metadata"]["devicePixelRatio"], 2);
            assert!(capture["metadata"]["clientViewport"]["width"]
                .as_u64()
                .unwrap()
                > 0);
            let saved = std::fs::read(
                manifest_path
                    .parent()
                    .unwrap()
                    .join(capture["path"].as_str().unwrap()),
            )
            .expect("manifest-listed PNG exists");
            assert!(saved.starts_with(PNG_SIGNATURE));
            assert_eq!(saved, inline_images[index], "saved PNG pairs with typed image by capture order");
        }
        assert!(manifest["captures"][1]["metadata"]["scroll"]["y"]
            .as_f64()
            .unwrap()
            > 0.0);
        let restoration = &manifest["restoration"];
        assert_eq!(restoration["success"], true);
        assert_eq!(
            restoration["actual"]["clientViewport"],
            restoration["expected"]["clientViewport"]
        );
        assert_eq!(
            restoration["actual"]["devicePixelRatio"],
            restoration["expected"]["devicePixelRatio"]
        );
        assert_eq!(
            restoration["actual"]["scroll"],
            restoration["expected"]["scroll"]
        );

        let (_restore_mismatch_wrapper, restore_mismatch_node) =
            make_node_wrapper(false, &[("CODEX_BROWSER_FIXTURE_MISMATCH_RESTORE", "1")]);
        let restore_mismatch_root = temp.path().join("restore-mismatch-state");
        let restore_mismatch = run_visual(
            restore_mismatch_root.clone(),
            "restore-mismatch-thread",
            json!({
                "captures": [{"label": "before-mismatch", "viewportWidth": 960, "scroll": "bottom"}],
                "save_artifact": true
            }),
            restore_mismatch_node,
        )
        .await
        .expect("restoration readback mismatch response");
        assert!(!restore_mismatch.success);
        let DynamicToolCallOutputContentItem::InputText { text } = &restore_mismatch.content_items[0]
        else {
            panic!("restoration mismatch includes structured text");
        };
        let restoration_json = text
            .lines()
            .find_map(|line| line.strip_prefix("restoration: "))
            .expect("actual and expected restoration readbacks are reported");
        let restoration: Value = serde_json::from_str(restoration_json).unwrap();
        assert_eq!(restoration["success"], false);
        assert_eq!(restoration["actual"]["devicePixelRatio"], 1.5);
        assert_eq!(restoration["expected"]["devicePixelRatio"], 2);
        assert_eq!(restoration["actual"]["clientViewport"]["width"], 1279);
        assert_eq!(restoration["expected"]["clientViewport"]["width"], 1280);
        let mismatch_profile = std::fs::read_dir(restore_mismatch_root.join("profiles"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert!(
            !mismatch_profile.join("artifacts").exists(),
            "no artifacts are saved when viewport restoration readback mismatches"
        );

        let (_invalid_capture_mode_wrapper, invalid_capture_mode_node) =
            make_node_wrapper(false, &[("CODEX_BROWSER_PLAYWRIGHT_CAPTURE_MODE", "typo")]);
        let invalid_capture_mode_root = temp.path().join("invalid-capture-mode-state");
        let invalid_capture_mode = run_visual(
            invalid_capture_mode_root.clone(),
            "invalid-capture-mode-thread",
            json!({}),
            invalid_capture_mode_node,
        )
        .await
        .expect("unsupported capture mode response");
        assert!(!invalid_capture_mode.success);
        assert!(matches!(
            invalid_capture_mode.content_items.first(),
            Some(DynamicToolCallOutputContentItem::InputText { text })
                if text.contains("unsupported capture mode")
        ));
        assert!(
            !invalid_capture_mode_root.exists(),
            "unsupported capture mode fails before creating a profile"
        );

        let invalid_root = temp.path().join("invalid-visual-state");
        let invalid_inputs = [
            (
                "bad-scope",
                json!({"scope": "whole_universe"}),
                "unsupported visual scope",
            ),
            (
                "bad-count",
                json!({"captures": [
                    {"label": "one"}, {"label": "two"}, {"label": "three"},
                    {"label": "four"}, {"label": "five"}
                ]}),
                "between one and four",
            ),
            (
                "bad-viewport",
                json!({"captures": [{"label": "bad", "viewportWidth": 0}]}),
                "viewportWidth must be an integer",
            ),
            (
                "bad-scroll-mode",
                json!({"captures": [{"label": "bad", "scroll": "middle"}]}),
                "capture scroll must be current, top, or bottom",
            ),
        ];
        for (thread, arguments, diagnostic) in invalid_inputs {
            let invalid = run_visual(
                invalid_root.clone(),
                thread,
                arguments,
                configured_node_path.clone(),
            )
            .await
            .expect("invalid visual argument response");
            assert!(!invalid.success);
            assert!(matches!(
                invalid.content_items.first(),
                Some(DynamicToolCallOutputContentItem::InputText { text }) if text.contains(diagnostic)
            ));
        }
        assert!(
            !invalid_root.exists(),
            "invalid visual arguments fail before profile side effects"
        );
    }

    #[tokio::test]
    async fn hosted_native_browser_tool_flow() {
        if std::env::var_os(ENV_PLAYWRIGHT_NODE_PATH).is_none()
            || std::env::var_os(ENV_PLAYWRIGHT_EXECUTABLE_PATH).is_none()
            || std::env::var_os(ENV_PLAYWRIGHT_STATE_DIR).is_none()
            || std::env::var_os(ENV_PLAYWRIGHT_HEADLESS).is_none()
        {
            return;
        }
        if let Ok(provider) = std::env::var(ENV_PROVIDER) {
            assert_eq!(provider, PROVIDER_PLAYWRIGHT);
        }

        async fn invoke(
            codex_home: &Path,
            thread_id: &str,
            tool: &str,
            arguments: Value,
        ) -> DynamicToolCallResponse {
            let params = DynamicToolCallParams {
                thread_id: thread_id.to_string(),
                turn_id: format!("{thread_id}-turn"),
                call_id: format!("{thread_id}-{tool}"),
                namespace: Some("codex_browser".to_string()),
                tool: tool.to_string(),
                arguments,
            };
            match handle_browser_computer_use_for_codex_home(&params, codex_home).await {
                BrowserComputerUseOutcome::Handled(response) => response,
                BrowserComputerUseOutcome::Unavailable => {
                    panic!("configured Playwright provider must handle {tool}")
                }
            }
        }

        fn decode_base64(encoded: &str) -> Vec<u8> {
            const ALPHABET: &[u8] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            let mut decoded = Vec::new();
            let mut accumulator = 0u32;
            let mut bits = 0u8;
            for byte in encoded.bytes().take_while(|byte| *byte != b'=') {
                let value = ALPHABET
                    .iter()
                    .position(|candidate| *candidate == byte)
                    .expect("valid base64 image data") as u32;
                accumulator = (accumulator << 6) | value;
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    decoded.push((accumulator >> bits) as u8);
                }
            }
            decoded
        }

        let codex_home = tempfile::tempdir().expect("disposable Codex home");
        std::fs::write(
            codex_home.path().join("browser-computer-use.json"),
            r#"{"provider":"playwright"}"#,
        )
        .expect("write disposable browser config");
        let url = "data:text/html,%3Chtml%3E%3Ctitle%3Ecodex-browser-hosted%3C/title%3E%3Cbutton%20id%3D%22safe%22%3Esafe%3C/button%3E%3C/html%3E";
        let state_root = PathBuf::from(
            std::env::var_os(ENV_PLAYWRIGHT_STATE_DIR).expect("disposable Playwright state dir"),
        );

        for thread_id in ["hosted-browser-thread-a", "hosted-browser-thread-b"] {
            let observed = invoke(
                codex_home.path(),
                thread_id,
                TOOL_BROWSER_OBSERVE,
                json!({"url": url}),
            )
            .await;
            assert!(observed.success, "{observed:?}");
            assert!(response_includes_native_image(&observed));
            assert!(observed.content_items.iter().any(|item| matches!(
                item,
                DynamicToolCallOutputContentItem::InputText { text }
                    if text.contains("codex-browser-hosted")
            )));

            let stepped = invoke(
                codex_home.path(),
                thread_id,
                TOOL_BROWSER_STEP,
                json!({"url": url, "actions": [{"type": "click", "selector": "#safe"}]}),
            )
            .await;
            assert!(stepped.success, "{stepped:?}");
            assert!(response_includes_native_image(&stepped));
            assert!(stepped.content_items.iter().any(|item| matches!(
                item,
                DynamicToolCallOutputContentItem::InputText { text }
                    if text.contains("clicked browser selector")
            )));
        }

        let html = "<html><head><title>visual fixture</title></head><body><button id='safe'>Safe button</button><input aria-label='Secret field' value='DO_NOT_EXPOSE_FIXTURE_SECRET'><div contenteditable='true'>DO_NOT_EXPOSE_EDITABLE_SECRET</div><div role='textbox' tabindex='0'>DO_NOT_EXPOSE_ARIA_TEXTBOX_SECRET</div><div role='combobox' tabindex='0'>DO_NOT_EXPOSE_ARIA_COMBOBOX_SECRET</div><div style='height:2400px'>Long static page</div></body></html>";
        let encoded_html = html
            .bytes()
            .map(|byte| format!("%{byte:02X}"))
            .collect::<String>();
        let visual_url = format!("data:text/html,{encoded_html}");
        let visual = invoke(
            codex_home.path(),
            "hosted-browser-visual-thread",
            TOOL_BROWSER_OBSERVE,
            json!({
                "url": visual_url.clone(),
                "scope": "viewport_and_page",
                "interaction_map": {"scope": "page", "offset": 0},
                "captures": [
                    {"label": "top-wide", "viewportWidth": 960, "viewportHeight": 640, "scroll": "top"},
                    {"label": "bottom-narrow", "viewportWidth": 640, "viewportHeight": 480, "scroll": "bottom"}
                ],
                "save_artifact": true
            }),
        )
        .await;
        assert!(visual.success, "{visual:?}");
        assert_eq!(
            visual
                .content_items
                .iter()
                .filter(|item| matches!(item, DynamicToolCallOutputContentItem::InputImage { .. }))
                .count(),
            2,
            "every labeled capture has one typed inline image"
        );
        let image_bytes = visual
            .content_items
            .iter()
            .filter_map(|item| match item {
                DynamicToolCallOutputContentItem::InputImage { image_url } => Some(decode_base64(
                    image_url
                        .strip_prefix("data:image/png;base64,")
                        .expect("typed PNG data URL"),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
        assert_eq!(image_bytes.len(), 2);
        for bytes in &image_bytes {
            assert!(bytes.len() > PNG_SIGNATURE.len());
            assert!(bytes.starts_with(PNG_SIGNATURE));
        }
        assert_ne!(
            image_bytes[0], image_bytes[1],
            "captures at different visual states must remain distinct"
        );
        let visual_text = visual
            .content_items
            .iter()
            .find_map(|item| match item {
                DynamicToolCallOutputContentItem::InputText { text } => Some(text.as_str()),
                _ => None,
            })
            .expect("visual metadata text");
        assert!(visual_text.contains("top-wide"));
        assert!(visual_text.contains("bottom-narrow"));
        assert!(visual_text.contains("page_hints:"));
        assert!(
            visual_text.contains("#safe"),
            "control hint includes an actionable selector"
        );
        assert!(visual_text.contains("restoration:"));
        assert!(visual_text.contains("\"success\":true"));
        assert!(visual_text.contains("\"width\":960"));
        assert!(visual_text.contains("\"width\":640"));
        assert!(!visual_text.contains("DO_NOT_EXPOSE_FIXTURE_SECRET"));
        assert!(!visual_text.contains("DO_NOT_EXPOSE_EDITABLE_SECRET"));
        assert!(!visual_text.contains("DO_NOT_EXPOSE_ARIA_TEXTBOX_SECRET"));
        assert!(!visual_text.contains("DO_NOT_EXPOSE_ARIA_COMBOBOX_SECRET"));
        assert!(!visual_text.contains("data:image/png;base64,"));

        let visual_profile = std::fs::read_dir(state_root.join("profiles"))
            .expect("provider profile directories")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|profile| profile.join("artifacts").is_dir())
            .expect("visual profile artifacts");
        let artifact_relative_path = visual_text
            .lines()
            .find_map(|line| line.strip_prefix("artifact_manifest: "))
            .expect("response includes the saved manifest path");
        let repeated_visual = invoke(
            codex_home.path(),
            "hosted-browser-visual-thread",
            TOOL_BROWSER_OBSERVE,
            json!({
                "url": visual_url.clone(),
                "captures": [
                    {"label": "top-wide", "viewportWidth": 960, "viewportHeight": 640, "scroll": "top"},
                    {"label": "bottom-narrow", "viewportWidth": 640, "viewportHeight": 480, "scroll": "bottom"}
                ],
                "save_artifact": true
            }),
        )
        .await;
        assert!(repeated_visual.success, "{repeated_visual:?}");
        let repeated_manifest_path = repeated_visual
            .content_items
            .iter()
            .find_map(|item| match item {
                DynamicToolCallOutputContentItem::InputText { text } => text
                    .lines()
                    .find_map(|line| line.strip_prefix("artifact_manifest: ")),
                _ => None,
            })
            .expect("second exclusive manifest path");
        assert_ne!(artifact_relative_path, repeated_manifest_path);
        assert_eq!(
            std::fs::read_dir(visual_profile.join("artifacts"))
                .unwrap()
                .count(),
            2,
            "repeated saves create distinct exclusive run directories"
        );
        let manifest_path = visual_profile.join(artifact_relative_path);
        let artifact_run = manifest_path
            .parent()
            .expect("manifest parent")
            .to_path_buf();
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("paired manifest"))
                .expect("valid artifact manifest");
        assert_eq!(manifest["captures"].as_array().unwrap().len(), 2);
        assert_eq!(manifest["captures"][0]["label"], "top-wide");
        assert_eq!(manifest["captures"][1]["label"], "bottom-narrow");
        assert_eq!(
            manifest["captures"][0]["metadata"]["effectiveViewport"]["width"],
            960
        );
        assert_eq!(
            manifest["captures"][1]["metadata"]["effectiveViewport"]["width"],
            640
        );
        for (index, expected_label) in ["top-wide", "bottom-narrow"].iter().enumerate() {
            let capture = &manifest["captures"][index];
            assert_eq!(capture["order"], index + 1);
            assert_eq!(capture["label"], *expected_label);
            for axis in ["width", "height"] {
                assert!(
                    capture["metadata"]["effectiveViewport"][axis]
                        .as_u64()
                        .unwrap()
                        > 0
                );
                assert!(
                    capture["metadata"]["clientViewport"][axis]
                        .as_u64()
                        .unwrap()
                        > 0
                );
                assert!(capture["metadata"]["document"][axis].as_u64().unwrap() > 0);
            }
            assert!(capture["metadata"]["devicePixelRatio"].as_f64().unwrap() > 0.0);
            if index == 1 {
                assert!(capture["metadata"]["scroll"]["y"].as_f64().unwrap() > 0.0);
            }
            let saved = std::fs::read(artifact_run.join(capture["path"].as_str().unwrap()))
                .expect("listed PNG exists");
            assert!(saved.len() > PNG_SIGNATURE.len());
            assert!(saved.starts_with(PNG_SIGNATURE));
            assert_eq!(
                saved, image_bytes[index],
                "manifest path pairs with the same typed image"
            );
        }
        let restoration = &manifest["restoration"];
        assert_eq!(restoration["success"], true);
        assert_eq!(
            restoration["actual"]["effectiveViewport"],
            restoration["expected"]["effectiveViewport"]
        );
        assert_eq!(
            restoration["actual"]["scroll"],
            restoration["expected"]["scroll"]
        );
        assert_eq!(
            std::fs::read_dir(state_root.join("profiles"))
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.path().join("artifacts").is_dir())
                .count(),
            1,
            "ordinary observes without save_artifact create no artifact directories"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(visual_profile.join("artifacts"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&artifact_run)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(artifact_run.join("manifest.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            for capture in manifest["captures"].as_array().unwrap() {
                assert_eq!(
                    std::fs::metadata(artifact_run.join(capture["path"].as_str().unwrap()))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
        }
        assert_eq!(std::fs::read_dir(&artifact_run).unwrap().count(), 3);

        let invalid = invoke(
            codex_home.path(),
            "hosted-browser-invalid-visual-thread",
            TOOL_BROWSER_OBSERVE,
            json!({"url": visual_url.clone(), "captures": [{"label": "same"}, {"label": "same"}]}),
        )
        .await;
        assert!(!invalid.success);
        assert!(matches!(
            invalid.content_items.first(),
            Some(DynamicToolCallOutputContentItem::InputText { text }) if text.contains("unique non-empty label")
        ));

        let profiles = std::fs::read_dir(state_root.join("profiles"))
            .expect("provider-created thread profiles")
            .count();
        assert_eq!(
            profiles, 3,
            "each thread, including the visual capture thread, receives its own profile"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn windows_platform_guard_rejects_before_state_creation() {
        let temp = tempfile::tempdir().expect("temporary fixture root");
        let preload = temp.path().join("windows-platform.cjs");
        std::fs::write(
            &preload,
            "Object.defineProperty(process, 'platform', { value: 'win32' });\n",
        )
        .expect("write child-only platform preload");
        let mut script = tempfile::Builder::new()
            .suffix(".mjs")
            .tempfile()
            .expect("temporary provider script");
        script
            .write_all(PLAYWRIGHT_BRIDGE_SCRIPT.as_bytes())
            .expect("write provider script");
        let state_dir = temp.path().join("must-not-be-created");
        let params = DynamicToolCallParams {
            thread_id: "thread-fixture".to_string(),
            turn_id: "turn-fixture".to_string(),
            call_id: "call-fixture".to_string(),
            namespace: Some("codex_browser".to_string()),
            tool: TOOL_BROWSER_OBSERVE.to_string(),
            arguments: json!({}),
        };
        let mut child = Command::new("node")
            .arg("--require")
            .arg(&preload)
            .arg(script.path())
            .env(ENV_PLAYWRIGHT_STATE_DIR, &state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start child with platform-only preload");
        child
            .stdin
            .take()
            .expect("provider stdin")
            .write_all(&serde_json::to_vec(&params).expect("serialize request"))
            .await
            .expect("write request");
        let output = child.wait_with_output().await.expect("wait for provider");
        assert!(output.status.success());
        let response: DynamicToolCallResponse =
            serde_json::from_slice(&output.stdout).expect("structured fail-closed response");
        assert!(!response.success);
        assert!(!state_dir.exists());
        assert!(matches!(
            response.content_items.first(),
            Some(DynamicToolCallOutputContentItem::InputText { text })
                if text.contains("unsupported on Windows")
        ));
    }
}
