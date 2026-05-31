use codex_app_server_protocol::ComputerUseCallOutputContentItem;
use codex_app_server_protocol::ComputerUseCallParams;
use codex_app_server_protocol::ComputerUseCallResponse;
use codex_app_server_protocol::DynamicToolSpec;
use codex_protocol::dynamic_tools::DynamicToolCapability;
use codex_tools::COMPUTER_USE_ADAPTER_DESKTOP;
use codex_tools::DESKTOP_OBSERVE_TOOL_NAME;
use codex_tools::DESKTOP_STEP_TOOL_NAME;
use codex_tools::native_computer_use_provider_for_call;
use serde::Deserialize;
use serde_json::json;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::time::timeout;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const ENV_COMMAND: &str = "CODEX_DESKTOP_COMPUTER_USE_COMMAND";
const ENV_PROVIDER: &str = "CODEX_DESKTOP_COMPUTER_USE_PROVIDER";
const ENV_TIMEOUT_SECS: &str = "CODEX_DESKTOP_COMPUTER_USE_TIMEOUT_SECS";
const PROVIDER_COMMAND: &str = "command";
const PROVIDER_NONE: &str = "none";

pub(crate) enum DesktopComputerUseOutcome {
    Handled(ComputerUseCallResponse),
    Unavailable,
}

pub(crate) fn configured_desktop_dynamic_tools() -> Vec<DynamicToolSpec> {
    if DesktopRuntimeConfig::load().is_none() {
        return Vec::new();
    }

    vec![
        desktop_dynamic_tool(
            DESKTOP_OBSERVE_TOOL_NAME,
            "Capture the current desktop app state as a model-visible screenshot.",
            "non_mutating",
        ),
        desktop_dynamic_tool(
            DESKTOP_STEP_TOOL_NAME,
            "Perform bounded desktop UI actions, then return a fresh desktop screenshot.",
            "mutating",
        ),
    ]
}

fn desktop_dynamic_tool(name: &str, description: &str, mutation_class: &str) -> DynamicToolSpec {
    DynamicToolSpec {
        namespace: None,
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "additionalProperties": true
        }),
        defer_loading: false,
        persist_on_resume: false,
        capability: Some(DynamicToolCapability {
            family: Some(COMPUTER_USE_ADAPTER_DESKTOP.to_string()),
            capability_scope: Some("session".to_string()),
            mutation_class: Some(mutation_class.to_string()),
            lease_mode: None,
        }),
    }
}

pub(crate) async fn handle_desktop_computer_use(
    params: &ComputerUseCallParams,
) -> DesktopComputerUseOutcome {
    if params.adapter != COMPUTER_USE_ADAPTER_DESKTOP
        || native_computer_use_provider_for_call(COMPUTER_USE_ADAPTER_DESKTOP, &params.tool)
            .is_none()
    {
        return DesktopComputerUseOutcome::Unavailable;
    }

    let Some(config) = DesktopRuntimeConfig::load() else {
        return DesktopComputerUseOutcome::Unavailable;
    };

    let request_timeout = config.timeout;
    let response = match timeout(request_timeout, handle_with_config(params, config)).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => failed_response(err),
        Err(_) => failed_response(format!(
            "Desktop computer-use provider timed out after {} seconds.",
            request_timeout.as_secs()
        )),
    };
    DesktopComputerUseOutcome::Handled(response)
}

async fn handle_with_config(
    params: &ComputerUseCallParams,
    config: DesktopRuntimeConfig,
) -> Result<ComputerUseCallResponse, String> {
    let mut response = run_command_provider(params, &config.command).await?;
    require_native_image_for_visual_response(
        &mut response,
        "Desktop observation missing native image output.",
    );
    Ok(response)
}

async fn run_command_provider(
    params: &ComputerUseCallParams,
    command: &CommandProviderConfig,
) -> Result<ComputerUseCallResponse, String> {
    let output = run_provider_process(&command.argv, params).await?;
    parse_provider_response(&output)
}

async fn run_provider_process(
    argv: &[String],
    params: &ComputerUseCallParams,
) -> Result<Vec<u8>, String> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| "Desktop provider command is empty.".to_string())?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| format!("failed to start desktop provider `{program}`: {err}"))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "failed to open desktop provider stdin".to_string())?;
    let body = serde_json::to_vec(params)
        .map_err(|err| format!("failed to serialize desktop provider request: {err}"))?;
    stdin
        .write_all(&body)
        .await
        .map_err(|err| format!("failed to write desktop provider request: {err}"))?;
    drop(stdin);

    let output = child
        .wait_with_output()
        .await
        .map_err(|err| format!("failed to wait for desktop provider: {err}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "Desktop provider exited with status {}: {}",
            output.status,
            compact_process_output(&stderr)
        ))
    }
}

fn parse_provider_response(bytes: &[u8]) -> Result<ComputerUseCallResponse, String> {
    serde_json::from_slice(bytes).map_err(|err| {
        let snippet = compact_process_output(&String::from_utf8_lossy(bytes));
        format!("failed to parse desktop provider response: {err}; stdout: {snippet}")
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DesktopRuntimeConfig {
    command: CommandProviderConfig,
    timeout: Duration,
}

impl DesktopRuntimeConfig {
    fn load() -> Option<Self> {
        Self::from_sources(DesktopRuntimeConfigFile::load(), DesktopRuntimeEnv::read())
    }

    fn from_sources(
        file: Option<DesktopRuntimeConfigFile>,
        env: DesktopRuntimeEnv,
    ) -> Option<Self> {
        let provider_name = env
            .provider
            .as_deref()
            .or_else(|| file.as_ref().and_then(|config| config.provider.as_deref()))
            .unwrap_or(PROVIDER_COMMAND);
        if provider_is_disabled(provider_name) || !provider_is_command(provider_name) {
            return None;
        }

        let env_command = env
            .command
            .clone()
            .and_then(|command| command_spec_to_argv(CommandSpec::String(command)));
        if file
            .as_ref()
            .is_some_and(|config| !config.matches_current_platform())
            && env_command.is_none()
        {
            return None;
        }

        let command = env_command.or_else(|| {
            file.as_ref()
                .filter(|config| config.matches_current_platform())
                .and_then(|config| config.command.clone().and_then(command_spec_to_argv))
        })?;
        let timeout = env
            .timeout_secs
            .or_else(|| {
                file.as_ref()
                    .filter(|config| config.matches_current_platform())
                    .and_then(|config| config.timeout_secs)
            })
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT);

        Some(Self {
            command: CommandProviderConfig { argv: command },
            timeout,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommandProviderConfig {
    argv: Vec<String>,
}

#[derive(Deserialize)]
struct DesktopRuntimeConfigFile {
    provider: Option<String>,
    command: Option<CommandSpec>,
    timeout_secs: Option<u64>,
    platforms: Option<Vec<String>>,
}

impl DesktopRuntimeConfigFile {
    fn load() -> Option<Self> {
        let home = dirs::home_dir()?;
        for path in [
            home.join(".codex/desktop-computer-use.json"),
            home.join(".codex/desktop-dynamic-tools.json"),
        ] {
            if let Ok(contents) = std::fs::read_to_string(path)
                && let Ok(config) = serde_json::from_str(&contents)
            {
                return Some(config);
            }
        }
        None
    }

    fn matches_current_platform(&self) -> bool {
        let Some(platforms) = &self.platforms else {
            return true;
        };
        platforms
            .iter()
            .any(|platform| platform_matches_current(platform))
    }
}

#[derive(Default)]
struct DesktopRuntimeEnv {
    provider: Option<String>,
    command: Option<String>,
    timeout_secs: Option<u64>,
}

impl DesktopRuntimeEnv {
    fn read() -> Self {
        Self {
            provider: first_env(&[ENV_PROVIDER]),
            command: first_env(&[ENV_COMMAND]),
            timeout_secs: first_env(&[ENV_TIMEOUT_SECS]).and_then(|value| value.parse().ok()),
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
    argv.first()
        .is_some_and(|program| !program.trim().is_empty())
        .then_some(argv)
}

fn first_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.trim().is_empty())
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

fn provider_is_disabled(provider: &str) -> bool {
    provider.trim().eq_ignore_ascii_case(PROVIDER_NONE)
}

fn provider_is_command(provider: &str) -> bool {
    provider.trim().eq_ignore_ascii_case(PROVIDER_COMMAND)
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
            "\n\n{missing_image_message} The desktop provider must return screenshots as native image content items rather than text-only summaries or artifact paths."
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
    use std::io::Write as _;

    #[test]
    fn command_spec_accepts_shell_like_string_and_array() {
        assert_eq!(
            command_spec_to_argv(CommandSpec::String("desktop-provider --stdio".to_string())),
            Some(vec!["desktop-provider".to_string(), "--stdio".to_string()])
        );
        assert_eq!(
            command_spec_to_argv(CommandSpec::Array(vec![
                "desktop-provider".to_string(),
                "--stdio".to_string()
            ])),
            Some(vec!["desktop-provider".to_string(), "--stdio".to_string()])
        );
    }

    #[test]
    fn configured_desktop_tools_are_session_scoped_native_tools() {
        let tools = [
            desktop_dynamic_tool(DESKTOP_OBSERVE_TOOL_NAME, "observe", "non_mutating"),
            desktop_dynamic_tool(DESKTOP_STEP_TOOL_NAME, "step", "mutating"),
        ];

        assert_eq!(tools[0].name, DESKTOP_OBSERVE_TOOL_NAME);
        assert_eq!(tools[1].name, DESKTOP_STEP_TOOL_NAME);
        assert!(tools.iter().all(|tool| tool.namespace.is_none()));
        assert!(tools.iter().all(|tool| !tool.defer_loading));
        assert!(tools.iter().all(|tool| !tool.persist_on_resume));
        assert!(tools.iter().all(|tool| {
            tool.capability
                .as_ref()
                .and_then(|capability| capability.family.as_deref())
                == Some(COMPUTER_USE_ADAPTER_DESKTOP)
        }));
    }

    #[test]
    fn desktop_config_respects_platform_filters() {
        let config = DesktopRuntimeConfig::from_sources(
            Some(DesktopRuntimeConfigFile {
                provider: Some(PROVIDER_COMMAND.to_string()),
                command: Some(CommandSpec::Array(vec!["desktop-provider".to_string()])),
                timeout_secs: Some(7),
                platforms: Some(vec!["all".to_string()]),
            }),
            DesktopRuntimeEnv::default(),
        )
        .expect("desktop provider config");

        assert_eq!(config.command.argv, vec!["desktop-provider".to_string()]);
        assert_eq!(config.timeout, Duration::from_secs(7));
    }

    #[test]
    fn desktop_env_command_overrides_file_platform_filter() {
        let config = DesktopRuntimeConfig::from_sources(
            Some(DesktopRuntimeConfigFile {
                provider: Some(PROVIDER_COMMAND.to_string()),
                command: Some(CommandSpec::Array(vec!["file-provider".to_string()])),
                timeout_secs: Some(7),
                platforms: Some(vec!["not-this-platform".to_string()]),
            }),
            DesktopRuntimeEnv {
                command: Some("env-provider --stdio".to_string()),
                timeout_secs: Some(9),
                ..Default::default()
            },
        )
        .expect("desktop env provider config");

        assert_eq!(
            config.command.argv,
            vec!["env-provider".to_string(), "--stdio".to_string()]
        );
        assert_eq!(config.timeout, Duration::from_secs(9));
    }

    #[test]
    fn desktop_unknown_provider_is_unavailable() {
        let config = DesktopRuntimeConfig::from_sources(
            Some(DesktopRuntimeConfigFile {
                provider: Some("native-magic".to_string()),
                command: Some(CommandSpec::Array(vec!["desktop-provider".to_string()])),
                timeout_secs: None,
                platforms: None,
            }),
            DesktopRuntimeEnv::default(),
        );

        assert_eq!(config, None);
    }

    #[test]
    fn desktop_provider_response_without_native_image_fails_loudly() {
        let mut response = ComputerUseCallResponse {
            content_items: vec![ComputerUseCallOutputContentItem::InputText {
                text: "Desktop observation\napp: Notes".to_string(),
            }],
            success: true,
            error: None,
        };

        require_native_image_for_visual_response(
            &mut response,
            "Desktop observation missing native image output.",
        );

        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("Desktop observation missing native image output.")
        );
        let ComputerUseCallOutputContentItem::InputText { text } = &response.content_items[0]
        else {
            panic!("expected text summary");
        };
        assert!(text.contains("app: Notes"));
        assert!(text.contains("must return screenshots as native image content items"));
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
{"contentItems":[{"type":"inputText","text":"Desktop observation from fake provider"},{"type":"inputImage","imageUrl":"data:image/png;base64,AAAA","detail":"high"}],"success":true}
JSON
"#,
            )
            .expect("write provider");
        let params = ComputerUseCallParams {
            thread_id: "thread-1".to_string(),
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            environment_id: Some("env-1".to_string()),
            adapter: COMPUTER_USE_ADAPTER_DESKTOP.to_string(),
            tool: DESKTOP_OBSERVE_TOOL_NAME.to_string(),
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
