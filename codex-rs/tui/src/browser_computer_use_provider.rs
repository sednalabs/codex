use codex_app_server_protocol::ComputerUseCallOutputContentItem;
use codex_app_server_protocol::ComputerUseCallParams;
use codex_app_server_protocol::ComputerUseCallResponse;
use serde::Deserialize;
use serde_json::Value;
use std::io::Write as _;
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
const PROVIDER_COMMAND: &str = "command";
const PROVIDER_NONE: &str = "none";
const PROVIDER_PLAYWRIGHT: &str = "playwright";
const TOOL_BROWSER_OBSERVE: &str = "browser_observe";
const TOOL_BROWSER_STEP: &str = "browser_step";
const BACKEND_AUTO: &str = "auto";
const BACKEND_WILDCARD: &str = "*";

const PLAYWRIGHT_BRIDGE_SCRIPT: &str = include_str!("browser_playwright_provider.mjs");

pub(crate) enum BrowserComputerUseOutcome {
    Handled(ComputerUseCallResponse),
    Unavailable,
}

pub(crate) async fn handle_browser_computer_use(
    params: &ComputerUseCallParams,
) -> BrowserComputerUseOutcome {
    if params.adapter != "browser"
        || !matches!(
            params.tool.as_str(),
            TOOL_BROWSER_OBSERVE | TOOL_BROWSER_STEP
        )
    {
        return BrowserComputerUseOutcome::Unavailable;
    }

    let Some(config) = BrowserRuntimeConfig::load() else {
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
    params: &ComputerUseCallParams,
    provider: ConfiguredBrowserProvider,
) -> Result<ComputerUseCallResponse, String> {
    let provider_id = provider.id.clone();
    let mut response = match provider.provider {
        BrowserProvider::Command(command) => run_command_provider(params, &command).await,
        BrowserProvider::Playwright(playwright) => {
            let backend = requested_backend(&params.arguments);
            if backend != BACKEND_AUTO {
                return Err(format!(
                    "Browser backend `{backend}` is not available from Playwright provider `{provider_id}`. Use backend=auto or configure `{ENV_COMMAND}` for a provider that owns `{backend}`."
                ));
            }
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
    params: &ComputerUseCallParams,
    command: &CommandProviderConfig,
) -> Result<ComputerUseCallResponse, String> {
    let output = run_provider_process(&command.argv, params, &[]).await?;
    parse_provider_response(&output)
}

async fn run_playwright_provider(
    params: &ComputerUseCallParams,
    config: &PlaywrightProviderConfig,
) -> Result<ComputerUseCallResponse, String> {
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
    let mut envs = Vec::new();
    if let Some(state_dir) = &config.state_dir {
        envs.push((ENV_PLAYWRIGHT_STATE_DIR.to_string(), state_dir.clone()));
    }
    if let Some(headless) = config.headless {
        envs.push((
            ENV_PLAYWRIGHT_HEADLESS.to_string(),
            if headless { "1" } else { "0" }.to_string(),
        ));
    }

    let output = run_provider_process(&[config.node.clone(), script_path], params, &envs).await?;
    parse_provider_response(&output)
}

async fn run_provider_process(
    argv: &[String],
    params: &ComputerUseCallParams,
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

fn parse_provider_response(bytes: &[u8]) -> Result<ComputerUseCallResponse, String> {
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
    fn load() -> Option<Self> {
        Self::from_sources(BrowserRuntimeConfigFile::load(), BrowserRuntimeEnv::read())
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
                        state_dir: env
                            .state_dir
                            .clone()
                            .or_else(|| file.as_ref().and_then(|config| config.state_dir.clone())),
                        headless: env
                            .headless
                            .or_else(|| file.as_ref().and_then(|config| config.headless)),
                    },
                    vec![BACKEND_AUTO.to_string()],
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
    state_dir: Option<String>,
    headless: Option<bool>,
}

#[derive(Deserialize)]
struct BrowserRuntimeConfigFile {
    provider: Option<String>,
    command: Option<CommandSpec>,
    node: Option<String>,
    timeout_secs: Option<u64>,
    state_dir: Option<String>,
    headless: Option<bool>,
    providers: Option<Vec<BrowserProviderConfigFile>>,
    routing: Option<BrowserRoutingConfigFile>,
}

impl BrowserRuntimeConfigFile {
    fn load() -> Option<Self> {
        let home = dirs::home_dir()?;
        for path in [
            home.join(".codex/browser-computer-use.json"),
            home.join(".codex/browser-dynamic-tools.json"),
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
    timeout_secs: Option<u64>,
    state_dir: Option<String>,
    headless: Option<bool>,
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
                vec![BACKEND_AUTO.to_string()]
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
                    state_dir: self.state_dir.clone().or_else(|| env.state_dir.clone()),
                    headless: self.headless.or(env.headless),
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
    timeout_secs: Option<u64>,
    state_dir: Option<String>,
    headless: Option<bool>,
}

impl BrowserRuntimeEnv {
    fn read() -> Self {
        Self {
            provider: first_env(&[ENV_PROVIDER]),
            command: first_env(&[ENV_COMMAND]),
            node: first_env(&[ENV_NODE]),
            timeout_secs: first_env(&[ENV_TIMEOUT_SECS]).and_then(|value| value.parse().ok()),
            state_dir: first_env(&[ENV_PLAYWRIGHT_STATE_DIR]),
            headless: first_env(&[ENV_PLAYWRIGHT_HEADLESS]).and_then(|value| parse_bool(&value)),
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

fn response_includes_native_image(response: &ComputerUseCallResponse) -> bool {
    response
        .content_items
        .iter()
        .any(|item| matches!(item, ComputerUseCallOutputContentItem::InputImage { .. }))
}

fn require_native_image_for_visual_response(
    response: &mut ComputerUseCallResponse,
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
    response.error = Some(match response.error.take() {
        Some(existing_error) if !existing_error.trim().is_empty() => {
            format!("{missing_image_message} Previous provider error: {existing_error}")
        }
        _ => missing_image_message.to_string(),
    });
}

fn append_text(items: &mut Vec<ComputerUseCallOutputContentItem>, extra: &str) {
    if let Some(ComputerUseCallOutputContentItem::InputText { text }) = items.first_mut() {
        text.push_str(extra);
    } else {
        items.insert(
            0,
            ComputerUseCallOutputContentItem::InputText {
                text: extra.trim().to_string(),
            },
        );
    }
}

fn failed_response(error: String) -> ComputerUseCallResponse {
    ComputerUseCallResponse {
        content_items: vec![ComputerUseCallOutputContentItem::InputText {
            text: error.clone(),
        }],
        success: false,
        error: Some(error),
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
                timeout_secs: None,
                state_dir: None,
                headless: None,
                providers: Some(vec![
                    BrowserProviderConfigFile {
                        id: Some("chrome-provider".to_string()),
                        provider: Some(PROVIDER_COMMAND.to_string()),
                        command: Some(CommandSpec::Array(vec![
                            "node".to_string(),
                            "chrome-provider.mjs".to_string(),
                        ])),
                        node: None,
                        timeout_secs: None,
                        state_dir: None,
                        headless: None,
                        backends: Some(vec!["chrome".to_string()]),
                        platforms: None,
                    },
                    BrowserProviderConfigFile {
                        id: Some("playwright".to_string()),
                        provider: Some(PROVIDER_PLAYWRIGHT.to_string()),
                        command: None,
                        node: Some("node".to_string()),
                        timeout_secs: None,
                        state_dir: None,
                        headless: None,
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
                timeout_secs: None,
                state_dir: None,
                headless: None,
                providers: Some(vec![
                    BrowserProviderConfigFile {
                        id: Some("first".to_string()),
                        provider: Some(PROVIDER_COMMAND.to_string()),
                        command: Some(CommandSpec::Array(vec!["first".to_string()])),
                        node: None,
                        timeout_secs: None,
                        state_dir: None,
                        headless: None,
                        backends: None,
                        platforms: None,
                    },
                    BrowserProviderConfigFile {
                        id: Some("second".to_string()),
                        provider: Some(PROVIDER_COMMAND.to_string()),
                        command: Some(CommandSpec::Array(vec!["second".to_string()])),
                        node: None,
                        timeout_secs: None,
                        state_dir: None,
                        headless: None,
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
    fn legacy_playwright_provider_only_claims_auto_backend() {
        let config = BrowserRuntimeConfig::from_sources(
            Some(BrowserRuntimeConfigFile {
                provider: Some(PROVIDER_PLAYWRIGHT.to_string()),
                command: None,
                node: Some("node".to_string()),
                timeout_secs: None,
                state_dir: None,
                headless: None,
                providers: None,
                routing: None,
            }),
            BrowserRuntimeEnv::default(),
        )
        .expect("playwright provider");

        assert!(config.provider_for_backend(BACKEND_AUTO).is_some());
        assert!(config.provider_for_backend("chrome").is_none());
    }

    #[test]
    fn browser_provider_response_preserves_native_image() {
        let mut response = ComputerUseCallResponse {
            content_items: vec![
                ComputerUseCallOutputContentItem::InputText {
                    text: "Browser observation".to_string(),
                },
                ComputerUseCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAAA".to_string(),
                    detail: Some("high".to_string()),
                },
            ],
            success: true,
            error: None,
        };

        require_native_image_for_visual_response(
            &mut response,
            "Browser observation missing native image output.",
        );

        assert!(response.success);
        assert_eq!(response.error, None);
    }

    #[test]
    fn browser_provider_response_without_native_image_fails_loudly() {
        let mut response = ComputerUseCallResponse {
            content_items: vec![ComputerUseCallOutputContentItem::InputText {
                text: "Browser observation\nurl: https://example.test".to_string(),
            }],
            success: true,
            error: None,
        };

        require_native_image_for_visual_response(
            &mut response,
            "Browser observation missing native image output.",
        );

        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("Browser observation missing native image output.")
        );
        let ComputerUseCallOutputContentItem::InputText { text } = &response.content_items[0]
        else {
            panic!("expected text summary");
        };
        assert!(text.contains("url: https://example.test"));
        assert!(text.contains("must return screenshots as native image content items"));
    }

    #[test]
    fn missing_native_image_diagnostic_is_visible_for_empty_response() {
        let mut response = ComputerUseCallResponse {
            content_items: vec![],
            success: true,
            error: None,
        };

        require_native_image_for_visual_response(
            &mut response,
            "Browser observation missing native image output.",
        );

        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("Browser observation missing native image output.")
        );
        assert_eq!(response.content_items.len(), 1);
        let ComputerUseCallOutputContentItem::InputText { text } = &response.content_items[0]
        else {
            panic!("expected text diagnostic");
        };
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
                ComputerUseCallOutputContentItem::InputText {
                    text: "Browser observation".to_string(),
                },
                ComputerUseCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAAA".to_string(),
                    detail: Some("high".to_string()),
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
        let params = ComputerUseCallParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            environment_id: Some("env-1".to_string()),
            adapter: "browser".to_string(),
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
}
