use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use codex_app_server_protocol::ComputerUseCallOutputContentItem;
use codex_app_server_protocol::ComputerUseCallParams;
use codex_app_server_protocol::ComputerUseCallResponse;
use codex_app_server_protocol::DynamicToolSpec;
use codex_protocol::dynamic_tools::DynamicToolCapability;
use codex_tools::ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME;
use codex_tools::ANDROID_OBSERVE_TOOL_NAME;
use codex_tools::ANDROID_STEP_TOOL_NAME;
use codex_tools::COMPUTER_USE_ADAPTER_ANDROID;
use codex_tools::native_computer_use_provider_for_call;
use reqwest::StatusCode;
use reqwest::header::ACCEPT;
use reqwest::header::CONTENT_TYPE;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderValue;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::timeout;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_MCP_URL_PATH: &str = "/mcp";
const INSPECT_UI_MAX_ATTEMPTS: usize = 3;
const INSPECT_UI_RETRY_DELAY: Duration = Duration::from_millis(250);
const INSTALL_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const MCP_TOOL_INTERACTIVE_SESSION_INSTALL_BUILD_FROM_RUN: &str =
    "interactive_session.install_build_from_run";

pub enum AndroidComputerUseOutcome {
    Handled(ComputerUseCallResponse),
    Unavailable,
}

/// Return Android computer-use dynamic tools for the process default Codex home.
pub fn configured_android_dynamic_tools() -> Vec<DynamicToolSpec> {
    let Some(codex_home) = default_codex_home() else {
        return Vec::new();
    };

    configured_android_dynamic_tools_for_codex_home(codex_home.as_path())
}

/// Return Android computer-use dynamic tools for a specific Codex home.
pub fn configured_android_dynamic_tools_for_codex_home(codex_home: &Path) -> Vec<DynamicToolSpec> {
    if AndroidRuntimeConfig::load(codex_home).is_none() {
        return Vec::new();
    }

    vec![
        android_dynamic_tool(
            ANDROID_OBSERVE_TOOL_NAME,
            "Capture the current Android screen as a model-visible screenshot.",
            "non_mutating",
        ),
        android_dynamic_tool(
            ANDROID_STEP_TOOL_NAME,
            "Perform bounded Android actions, then return a fresh Android screenshot.",
            "mutating",
        ),
        android_dynamic_tool(
            ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME,
            "Install a GitHub Actions Android build into the active Android session.",
            "mutating",
        ),
    ]
}

fn android_dynamic_tool(name: &str, description: &str, mutation_class: &str) -> DynamicToolSpec {
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
            family: Some(COMPUTER_USE_ADAPTER_ANDROID.to_string()),
            capability_scope: Some("session".to_string()),
            mutation_class: Some(mutation_class.to_string()),
            lease_mode: None,
        }),
    }
}

pub async fn handle_android_computer_use(
    params: &ComputerUseCallParams,
) -> AndroidComputerUseOutcome {
    let Some(codex_home) = default_codex_home() else {
        return AndroidComputerUseOutcome::Unavailable;
    };

    handle_android_computer_use_for_codex_home(params, codex_home.as_path()).await
}

pub async fn handle_android_computer_use_for_codex_home(
    params: &ComputerUseCallParams,
    codex_home: &Path,
) -> AndroidComputerUseOutcome {
    if params.adapter != COMPUTER_USE_ADAPTER_ANDROID {
        return AndroidComputerUseOutcome::Unavailable;
    }

    if !is_supported_android_tool(&params.tool) {
        return AndroidComputerUseOutcome::Unavailable;
    }

    let Some(config) = AndroidRuntimeConfig::load(codex_home) else {
        return AndroidComputerUseOutcome::Unavailable;
    };

    let request_timeout = request_timeout_for_tool(&params.tool);
    let response = match timeout(request_timeout, handle_with_config(params, config)).await {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => failed_response(err),
        Err(_) => failed_response(format!(
            "Android computer-use provider timed out after {} seconds.",
            request_timeout.as_secs()
        )),
    };
    AndroidComputerUseOutcome::Handled(response)
}

fn default_codex_home() -> Option<PathBuf> {
    codex_utils_home_dir::find_codex_home()
        .ok()
        .map(PathBuf::from)
}

fn is_supported_android_tool(tool: &str) -> bool {
    native_computer_use_provider_for_call(COMPUTER_USE_ADAPTER_ANDROID, tool).is_some()
}

fn request_timeout_for_tool(tool: &str) -> Duration {
    match tool {
        ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME => INSTALL_REQUEST_TIMEOUT,
        _ => DEFAULT_REQUEST_TIMEOUT,
    }
}

async fn handle_with_config(
    params: &ComputerUseCallParams,
    config: AndroidRuntimeConfig,
) -> Result<ComputerUseCallResponse, String> {
    let defaults = config.defaults.clone();
    let mut client = AndroidRuntimeClient::connect(config).await?;
    let tools = client.list_tools().await?;

    let response = match params.tool.as_str() {
        ANDROID_OBSERVE_TOOL_NAME => {
            observe(&mut client, &tools, &defaults, &params.arguments).await
        }
        ANDROID_STEP_TOOL_NAME => step(&mut client, &tools, &defaults, &params.arguments).await,
        ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME => {
            install_build_from_run(&mut client, &tools, &defaults, &params.arguments).await
        }
        _ => Err(format!(
            "Unsupported Android computer-use tool `{}`.",
            params.tool
        )),
    };
    client.close().await;
    response
}

async fn observe(
    client: &mut AndroidRuntimeClient,
    tools: &BTreeSet<String>,
    defaults: &AndroidProviderDefaults,
    arguments: &Value,
) -> Result<ComputerUseCallResponse, String> {
    let mut response = match observe_ui(client, tools, defaults, arguments).await {
        Ok(observation) => {
            observation_response(client, tools, observation, "Android observation").await
        }
        Err(err) => {
            screenshot_fallback_response(
                client,
                tools,
                defaults,
                arguments,
                "Android observation",
                &err,
                /*action_already_executed*/ false,
            )
            .await
        }
    }?;
    require_native_image_for_visual_response(
        &mut response,
        "Android observation missing native image output. Text and visible_ui summaries are not sufficient for native computer use.",
    );
    Ok(response)
}

async fn step(
    client: &mut AndroidRuntimeClient,
    tools: &BTreeSet<String>,
    defaults: &AndroidProviderDefaults,
    arguments: &Value,
) -> Result<ComputerUseCallResponse, String> {
    let actions = canonical_actions(arguments);
    if actions.is_empty() {
        return Err("android_step requires an action or non-empty actions array.".to_string());
    }

    let mut summaries = Vec::new();
    for action in actions {
        match run_action(client, tools, defaults, &action).await {
            Ok(summary) => summaries.push(summary),
            Err(err) => {
                return Ok(action_failure_response(
                    client, tools, defaults, arguments, &action, err, &summaries,
                )
                .await);
            }
        }
    }

    let mut response = match observe_ui(client, tools, defaults, arguments).await {
        Ok(observation) => {
            observation_response(
                client,
                tools,
                observation,
                "Android post-action observation",
            )
            .await?
        }
        Err(err) => {
            screenshot_fallback_response(
                client,
                tools,
                defaults,
                arguments,
                "Android post-action observation",
                &err,
                /*action_already_executed*/ true,
            )
            .await?
        }
    };
    if let Some(ComputerUseCallOutputContentItem::InputText { text }) =
        response.content_items.first_mut()
    {
        let action_text = summaries
            .iter()
            .map(|summary| format!("- {summary}"))
            .collect::<Vec<_>>()
            .join("\n");
        *text = format!("Executed Android actions:\n{action_text}\n\n{text}");
    }
    require_native_image_for_visual_response(
        &mut response,
        "Android post-action observation missing native image output. The actions above may already have executed; recover with a fresh android_observe before making visual claims, and do not repeat mutating actions solely because the screenshot was missing.",
    );
    Ok(response)
}

async fn action_failure_response(
    client: &mut AndroidRuntimeClient,
    tools: &BTreeSet<String>,
    defaults: &AndroidProviderDefaults,
    arguments: &Value,
    failed_action: &Value,
    action_error: String,
    completed_summaries: &[String],
) -> ComputerUseCallResponse {
    let failure_summary = action_failure_summary(
        action_kind(failed_action),
        &action_error,
        completed_summaries,
    );
    let recovery_hint = "The current Android state is included below when available. The failed action may already have changed the device state; inspect the post-failure screen before retrying mutating input.";

    let mut response = match observe_ui(client, tools, defaults, arguments).await {
        Ok(observation) => {
            match observation_response(
                client,
                tools,
                observation,
                "Android action failure observation",
            )
            .await
            {
                Ok(response) => response,
                Err(err) => failed_response(format!(
                    "{failure_summary}\n\nPost-failure observation could not be converted into a model response: {err}\n\n{recovery_hint}"
                )),
            }
        }
        Err(observe_err) => {
            match screenshot_fallback_response(
                client,
                tools,
                defaults,
                arguments,
                "Android action failure observation",
                &observe_err,
                /*action_already_executed*/ true,
            )
            .await
            {
                Ok(response) => response,
                Err(fallback_err) => failed_response(format!(
                    "{failure_summary}\n\nPost-failure observation unavailable.\n\nandroid.inspect_ui failed: {observe_err}\nScreenshot fallback failed: {fallback_err}\n\n{recovery_hint}"
                )),
            }
        }
    };

    prepend_text(
        &mut response.content_items,
        &format!("{failure_summary}\n{recovery_hint}"),
    );
    require_native_image_for_visual_response(
        &mut response,
        "Android action failure observation missing native image output. The failed action may already have changed the device state; recover with a fresh android_observe before retrying mutating input.",
    );
    response.success = false;
    response.error = Some(format!(
        "Android action `{}` failed: {}",
        action_kind(failed_action),
        compact_summary_text(&action_error)
    ));
    response
}

async fn install_build_from_run(
    client: &mut AndroidRuntimeClient,
    tools: &BTreeSet<String>,
    defaults: &AndroidProviderDefaults,
    arguments: &Value,
) -> Result<ComputerUseCallResponse, String> {
    if !tools.contains(MCP_TOOL_INTERACTIVE_SESSION_INSTALL_BUILD_FROM_RUN) {
        return Err(format!(
            "Android provider does not expose `{MCP_TOOL_INTERACTIVE_SESSION_INSTALL_BUILD_FROM_RUN}`."
        ));
    }

    let install_result = client
        .call_tool(
            MCP_TOOL_INTERACTIVE_SESSION_INSTALL_BUILD_FROM_RUN,
            arguments_with_default_serial(arguments, defaults),
        )
        .await?;
    let install_summary = summarize_install_result(install_result.structured_content());

    let mut response = match observe_ui(client, tools, defaults, arguments).await {
        Ok(observation) => {
            observation_response(
                client,
                tools,
                observation,
                "Android post-install observation",
            )
            .await?
        }
        Err(err) => {
            match screenshot_fallback_response(
                client,
                tools,
                defaults,
                arguments,
                "Android post-install observation",
                &err,
                /*action_already_executed*/ true,
            )
            .await
            {
                Ok(response) => response,
                Err(fallback_err) => failed_response(format!(
                    "Android post-install observation degraded after install/build action completed.\n\nandroid.inspect_ui failed: {err}\nScreenshot fallback failed: {fallback_err}\n\nRecover with a fresh android_observe before making visual claims, and do not repeat the install solely because the screenshot was missing."
                )),
            }
        }
    };

    if let Some(ComputerUseCallOutputContentItem::InputText { text }) =
        response.content_items.first_mut()
    {
        *text = format!("{install_summary}\n\n{text}");
    }
    require_native_image_for_visual_response(
        &mut response,
        "Android post-install observation missing native image output. The install/build action may already have completed; recover with a fresh android_observe before making visual claims, and do not repeat the install solely because the screenshot was missing.",
    );
    Ok(response)
}

async fn observe_ui(
    client: &mut AndroidRuntimeClient,
    tools: &BTreeSet<String>,
    defaults: &AndroidProviderDefaults,
    arguments: &Value,
) -> Result<AndroidToolResult, String> {
    let tool_name = if tools.contains("android.wait_for_stable_ui") && prefer_stable_ui(arguments) {
        "android.wait_for_stable_ui"
    } else {
        "android.inspect_ui"
    };

    let mut inspect_args = json!({
        "include_screenshot": true,
    });
    copy_serial_or_default(arguments, &mut inspect_args, defaults);
    copy_if_present(arguments, &mut inspect_args, "timeout_secs");
    copy_if_present(arguments, &mut inspect_args, "screenshot_filename");
    copy_if_present(arguments, &mut inspect_args, "hierarchy_filename");
    copy_if_present(arguments, &mut inspect_args, "poll_interval_ms");
    copy_if_present(arguments, &mut inspect_args, "stable_polls");

    for attempt in 0..INSPECT_UI_MAX_ATTEMPTS {
        match client.call_tool(tool_name, inspect_args.clone()).await {
            Ok(observation) => return Ok(observation),
            Err(err)
                if attempt + 1 < INSPECT_UI_MAX_ATTEMPTS && should_retry_inspect_ui_error(&err) =>
            {
                tokio::time::sleep(INSPECT_UI_RETRY_DELAY).await;
            }
            Err(err) => return Err(err),
        }
    }

    Err(format!("{tool_name} failed after maximum retry attempts"))
}

fn prefer_stable_ui(arguments: &Value) -> bool {
    arguments
        .get("stable")
        .or_else(|| arguments.get("wait_for_stable_ui"))
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn should_retry_inspect_ui_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("ui hierarchy capture was unavailable")
        || normalized.contains("retry observation")
        || normalized.contains("uiautomator")
        || normalized.contains("window-dump")
        || normalized.contains("failed to stat remote object")
        || normalized.contains("no such file or directory")
}

async fn observation_response(
    client: &mut AndroidRuntimeClient,
    tools: &BTreeSet<String>,
    observation: AndroidToolResult,
    title: &str,
) -> Result<ComputerUseCallResponse, String> {
    let AndroidToolResult {
        structured: structured_observation,
        content,
    } = observation;
    let mut items = vec![ComputerUseCallOutputContentItem::InputText {
        text: summarize_observation(title, &structured_observation),
    }];

    append_mcp_image_content(&mut items, content);

    if tools.contains("android.read_artifact")
        && !items_include_native_image(&items)
        && let Some(path) = screenshot_path(&structured_observation)
    {
        match client
            .call_tool("android.read_artifact", json!({ "path": path }))
            .await
            .and_then(|value| artifact_bytes(value.structured_content()))
        {
            Ok(bytes) => {
                append_text(&mut items, "\nscreenshot: included as native image output");
                items.push(ComputerUseCallOutputContentItem::InputImage {
                    image_url: format!("data:image/png;base64,{}", BASE64_STANDARD.encode(bytes)),
                    detail: Some("high".to_string()),
                });
            }
            Err(err) => {
                append_text(
                    &mut items,
                    &format!(
                        "\n\nScreenshot could not be included as native image output from provider artifact `{path}`: {err}"
                    ),
                );
            }
        }
    }

    Ok(ComputerUseCallResponse {
        content_items: items,
        success: true,
        error: None,
    })
}

async fn screenshot_fallback_response(
    client: &mut AndroidRuntimeClient,
    tools: &BTreeSet<String>,
    defaults: &AndroidProviderDefaults,
    arguments: &Value,
    title: &str,
    observe_error: &str,
    action_already_executed: bool,
) -> Result<ComputerUseCallResponse, String> {
    let mut lines = vec![
        format!("{title} degraded"),
        format!("UI digest unavailable: {observe_error}"),
    ];

    let mut observation = json!({
        "node_count": 0_u64,
        "nodes": [],
    });
    let mut mcp_content = Vec::new();

    if tools.contains("android.capture_screenshot") {
        let mut args = json!({});
        copy_serial_or_default(arguments, &mut args, defaults);
        copy_inspect_screenshot_filename_for_capture(arguments, &mut args);
        match client.call_tool("android.capture_screenshot", args).await {
            Ok(capture) => {
                if let Some(serial) = capture
                    .structured_content()
                    .get("serial")
                    .and_then(Value::as_str)
                {
                    observation["serial"] = Value::String(serial.to_string());
                }
                if let Some(path) = screenshot_path(capture.structured_content()) {
                    observation["artifacts"] = json!({ "screenshot_path": path });
                    lines.push("native screenshot fallback captured".to_string());
                }
                mcp_content = capture.content;
            }
            Err(err) => {
                lines.push(format!("native screenshot fallback failed: {err}"));
            }
        }
    } else {
        lines.push("native screenshot fallback unavailable from provider".to_string());
    }

    let mut response = observation_response(
        client,
        tools,
        AndroidToolResult::new(observation, mcp_content),
        &lines.join("\n"),
    )
    .await?;
    response.success = action_already_executed || response_includes_native_image(&response);
    if !response.success {
        response.error = Some(observe_error.to_string());
    }
    Ok(response)
}

async fn run_action(
    client: &mut AndroidRuntimeClient,
    tools: &BTreeSet<String>,
    defaults: &AndroidProviderDefaults,
    action: &Value,
) -> Result<String, String> {
    match action_kind(action) {
        "launch_app" => {
            let args = launch_app_args(action, defaults)?;
            client.call_tool("android.launch_app", args).await?;
            Ok("launched Android app".to_string())
        }
        "tap" | "click" => {
            if has_xy(action) {
                client
                    .call_tool(
                        "android.input.tap",
                        input_args(action, &["x", "y"], defaults),
                    )
                    .await?;
                Ok(format!(
                    "tapped at {},{}",
                    value_display(action.get("x")),
                    value_display(action.get("y"))
                ))
            } else {
                let args = element_args(action, defaults)?;
                client.call_tool("android.tap_element", args).await?;
                Ok("tapped matching UI element".to_string())
            }
        }
        "double_click" => {
            if !has_xy(action) {
                return Err("double_click requires x and y coordinates.".to_string());
            }
            if tools.contains("android.input.double_tap") {
                client
                    .call_tool(
                        "android.input.double_tap",
                        input_args(action, &["x", "y"], defaults),
                    )
                    .await?;
            } else {
                let args = input_args(action, &["x", "y"], defaults);
                client.call_tool("android.input.tap", args.clone()).await?;
                tokio::time::sleep(Duration::from_millis(100)).await;
                client.call_tool("android.input.tap", args).await?;
            }
            Ok(format!(
                "double tapped at {},{}",
                value_display(action.get("x")),
                value_display(action.get("y"))
            ))
        }
        "long_press" => {
            if !tools.contains("android.input.long_press") {
                return Err("Android provider does not expose long-press input.".to_string());
            }
            client
                .call_tool(
                    "android.input.long_press",
                    input_args(action, &["x", "y", "duration_ms"], defaults),
                )
                .await?;
            Ok("long pressed Android coordinates".to_string())
        }
        "swipe" | "drag" => {
            client
                .call_tool(
                    "android.input.swipe",
                    input_args(action, &["x1", "y1", "x2", "y2", "duration_ms"], defaults),
                )
                .await?;
            Ok("swiped Android screen".to_string())
        }
        "multi_touch" => {
            if !tools.contains("android.input.multi_touch") {
                return Err(
                    "Android provider does not expose atomic multi-touch input; operator action is required to install or select a compatible provider. Sequential single-touch fallback is intentionally unavailable."
                        .to_string(),
                );
            }
            let args = multi_touch_args(action, defaults)?;
            let pointer_count = args
                .get("pointers")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            client.call_tool("android.input.multi_touch", args).await?;
            Ok(format!(
                "sent atomic Android multi-touch gesture with {pointer_count} pointers"
            ))
        }
        "scroll" => {
            let args = scroll_args(action, defaults)?;
            client.call_tool("android.input.swipe", args).await?;
            Ok("scrolled Android screen".to_string())
        }
        "type" | "type_text" => {
            if action.get("selector").is_some() || action.get("target").is_some() {
                let mut args = element_args(action, defaults)?;
                copy_if_present(action, &mut args, "text");
                client.call_tool("android.type_into_element", args).await?;
                Ok("typed into matching UI element".to_string())
            } else {
                let mut args = json!({});
                copy_if_present(action, &mut args, "text");
                copy_serial_or_default(action, &mut args, defaults);
                copy_if_present(action, &mut args, "wait_for_selector");
                copy_if_present(action, &mut args, "timeout_secs");
                client.call_tool("android.input.text", args).await?;
                Ok("typed Android text".to_string())
            }
        }
        "keypress" | "key" => {
            if tools.contains("android.input.keycombination")
                && action.get("keys").and_then(Value::as_array).is_some()
            {
                let mut args = json!({});
                copy_if_present(action, &mut args, "keys");
                copy_serial_or_default(action, &mut args, defaults);
                client
                    .call_tool("android.input.keycombination", args)
                    .await?;
                Ok("sent Android key combination".to_string())
            } else {
                let mut args = json!({});
                copy_first_present(action, &mut args, &["keycode", "key"]);
                copy_serial_or_default(action, &mut args, defaults);
                copy_if_present(action, &mut args, "wait_for_activity");
                copy_if_present(action, &mut args, "wait_for_package");
                copy_if_present(action, &mut args, "wait_for_selector");
                copy_if_present(action, &mut args, "timeout_secs");
                client.call_tool("android.input.keyevent", args).await?;
                Ok("sent Android key event".to_string())
            }
        }
        "wait" => {
            if let Some(ms) = action
                .get("ms")
                .or_else(|| action.get("wait_ms"))
                .and_then(Value::as_u64)
            {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                Ok(format!("waited {ms} ms"))
            } else {
                let mut args = json!({ "include_screenshot": true });
                copy_serial_or_default(action, &mut args, defaults);
                copy_if_present(action, &mut args, "timeout_secs");
                client.call_tool("android.wait_for_stable_ui", args).await?;
                Ok("waited for stable Android UI".to_string())
            }
        }
        "semantic_action" => {
            if !tools.contains("solarlab.semantic_action") {
                return Err("Android provider does not expose app semantic actions.".to_string());
            }
            client
                .call_tool("solarlab.semantic_action", action.clone())
                .await?;
            Ok("ran app semantic action".to_string())
        }
        other => Err(format!("Unsupported Android action `{other}`.")),
    }
}

fn action_kind(action: &Value) -> &str {
    action
        .get("type")
        .or_else(|| action.get("action"))
        .or_else(|| action.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("unknown")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AndroidRuntimeConfig {
    mcp_url: String,
    cf_access_client_id: Option<String>,
    cf_access_client_secret: Option<String>,
    defaults: AndroidProviderDefaults,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct AndroidProviderDefaults {
    serial: Option<String>,
    package_name: Option<String>,
    activity: Option<String>,
}

impl AndroidRuntimeConfig {
    fn load(codex_home: &Path) -> Option<Self> {
        Self::load_with_env(codex_home, first_env)
    }

    fn load_with_env(
        codex_home: &Path,
        mut env_lookup: impl FnMut(&[&str]) -> Option<String>,
    ) -> Option<Self> {
        let file = AndroidRuntimeConfigFile::load(codex_home);
        let mcp_url = env_lookup(&["CODEX_ANDROID_MCP_URL", "SOLARLAB_ANDROID_MCP_URL"])
            .or_else(|| {
                env_lookup(&[
                    "CODEX_ANDROID_MCP_HOSTNAME",
                    "SOLARLAB_ANDROID_MCP_HOSTNAME",
                ])
                .map(|host| {
                    let host = host.trim_end_matches('/');
                    if host.starts_with("http://") || host.starts_with("https://") {
                        format!("{host}{DEFAULT_MCP_URL_PATH}")
                    } else {
                        format!("https://{host}{DEFAULT_MCP_URL_PATH}")
                    }
                })
            })
            .or_else(|| file.as_ref().and_then(|config| config.mcp_url.clone()))?;
        Some(Self {
            mcp_url,
            cf_access_client_id: env_lookup(&[
                "CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_ID",
                "SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_ID",
            ]),
            cf_access_client_secret: env_lookup(&[
                "CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET",
                "SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET",
            ]),
            defaults: AndroidProviderDefaults {
                serial: env_lookup(&[
                    "CODEX_ANDROID_DEFAULT_SERIAL",
                    "SOLARLAB_ANDROID_DEFAULT_SERIAL",
                ])
                .or_else(|| {
                    file.as_ref()
                        .and_then(|config| non_blank_config_value(&config.default_serial))
                }),
                package_name: env_lookup(&[
                    "CODEX_ANDROID_DEFAULT_PACKAGE_NAME",
                    "SOLARLAB_ANDROID_DEFAULT_PACKAGE_NAME",
                ])
                .or_else(|| {
                    file.as_ref()
                        .and_then(|config| non_blank_config_value(&config.default_package_name))
                }),
                activity: env_lookup(&[
                    "CODEX_ANDROID_DEFAULT_ACTIVITY",
                    "SOLARLAB_ANDROID_DEFAULT_ACTIVITY",
                ])
                .or_else(|| {
                    file.as_ref()
                        .and_then(|config| non_blank_config_value(&config.default_activity))
                }),
            },
        })
    }
}

fn non_blank_config_value(value: &Option<String>) -> Option<String> {
    value
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .cloned()
}

#[derive(serde::Deserialize)]
struct AndroidRuntimeConfigFile {
    mcp_url: Option<String>,
    default_serial: Option<String>,
    default_package_name: Option<String>,
    default_activity: Option<String>,
}

impl AndroidRuntimeConfigFile {
    fn load(codex_home: &Path) -> Option<Self> {
        for path in [
            codex_home.join("android-computer-use.json"),
            codex_home.join("android-dynamic-tools.json"),
            codex_home.join("solarlab-android-dynamic-tools.json"),
        ] {
            if let Ok(contents) = std::fs::read_to_string(path)
                && let Ok(config) = serde_json::from_str(&contents)
            {
                return Some(config);
            }
        }
        None
    }
}

struct AndroidRuntimeClient {
    http: reqwest::Client,
    url: String,
    headers: HeaderMap,
    session_id: Option<String>,
    next_id: u64,
}

#[derive(Debug, Clone, PartialEq)]
struct AndroidToolResult {
    structured: Value,
    content: Vec<Value>,
}

impl AndroidToolResult {
    fn new(structured: Value, content: Vec<Value>) -> Self {
        Self {
            structured,
            content,
        }
    }

    fn structured_content(&self) -> &Value {
        &self.structured
    }
}

impl AndroidRuntimeClient {
    async fn connect(config: AndroidRuntimeConfig) -> Result<Self, String> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/json, text/event-stream"),
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        match (&config.cf_access_client_id, &config.cf_access_client_secret) {
            (Some(id), Some(secret)) => {
                headers.insert(
                    "CF-Access-Client-Id",
                    HeaderValue::from_str(id)
                        .map_err(|err| format!("invalid Cloudflare Access client id: {err}"))?,
                );
                headers.insert(
                    "CF-Access-Client-Secret",
                    HeaderValue::from_str(secret)
                        .map_err(|err| format!("invalid Cloudflare Access client secret: {err}"))?,
                );
            }
            (None, None) => {}
            _ => {
                return Err(
                    "Both Cloudflare Access client id and secret must be set for Android provider."
                        .to_string(),
                );
            }
        }

        let mut client = Self {
            http: reqwest::Client::new(),
            url: config.mcp_url,
            headers,
            session_id: None,
            next_id: 1,
        };
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "codex-tui-native-android",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }),
            )
            .await?;
        let _ = client.notify("notifications/initialized").await;
        Ok(client)
    }

    async fn list_tools(&mut self) -> Result<BTreeSet<String>, String> {
        let value = self.request("tools/list", json!({})).await?;
        Ok(value
            .get("tools")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .map(ToString::to_string)
            .collect())
    }

    async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
    ) -> Result<AndroidToolResult, String> {
        let value = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        if value.get("isError").and_then(Value::as_bool) == Some(true) {
            return Err(tool_text(&value).unwrap_or_else(|| format!("tool `{name}` failed")));
        }
        Ok(tool_result(value))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let response = self.post(body).await?;
        if let Some(error) = response.get("error") {
            return Err(format!(
                "Android provider `{method}` returned error: {error}"
            ));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("Android provider `{method}` response omitted result"))
    }

    async fn notify(&mut self, method: &str) -> Result<(), String> {
        self.post(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": {},
        }))
        .await
        .map(|_| ())
    }

    async fn post(&mut self, body: Value) -> Result<Value, String> {
        let mut request = self
            .http
            .post(&self.url)
            .headers(self.headers.clone())
            .json(&body);
        if let Some(session_id) = &self.session_id {
            request = request.header("mcp-session-id", session_id);
        }
        let response = request
            .send()
            .await
            .map_err(|err| format!("failed to reach Android provider: {err}"))?;
        if let Some(session_id) = response.headers().get("mcp-session-id")
            && let Ok(session_id) = session_id.to_str()
        {
            self.session_id = Some(session_id.to_string());
        }
        let status = response.status();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = response
            .text()
            .await
            .map_err(|err| format!("failed to read Android provider response: {err}"))?;
        if !status.is_success() {
            return Err(format_http_error(status, &text));
        }
        if content_type.contains("text/event-stream") {
            parse_event_stream_json(&text)
        } else {
            serde_json::from_str(&text)
                .map_err(|err| format!("failed to parse Android provider JSON response: {err}"))
        }
    }

    async fn close(&mut self) {
        if let Some(session_id) = self.session_id.take() {
            let _ = self
                .http
                .delete(&self.url)
                .headers(self.headers.clone())
                .header("mcp-session-id", session_id)
                .send()
                .await;
        }
    }
}

fn canonical_actions(arguments: &Value) -> Vec<Value> {
    arguments
        .get("actions")
        .and_then(Value::as_array)
        .filter(|actions| !actions.is_empty())
        .cloned()
        .unwrap_or_else(|| vec![arguments.clone()])
}

fn summarize_observation(title: &str, observation: &Value) -> String {
    let mut lines = vec![title.to_string()];
    if let Some(serial) = observation.get("serial").and_then(Value::as_str) {
        lines.push(format!("serial: {serial}"));
    }
    if let Some(node_count) = observation.get("node_count").and_then(Value::as_u64) {
        lines.push(format!("node_count: {node_count}"));
    }
    if let Some(focus) = observation.get("current_focus") {
        lines.push(format!("current_focus: {}", compact_json(focus)));
    }
    if let Some(window_state) = observation.get("window_state") {
        lines.push(format!("window_state: {}", compact_json(window_state)));
    }
    let labels = observation_labels(observation);
    if !labels.is_empty() {
        lines.push("visible_ui:".to_string());
        lines.extend(labels.into_iter().map(|label| format!("- {label}")));
    }
    lines.join("\n")
}

fn summarize_install_result(result: &Value) -> String {
    let mut lines = vec!["Android build install".to_string()];
    push_bool_line(&mut lines, result, "ok");
    push_bool_line(&mut lines, result, "installed");
    push_bool_line(&mut lines, result, "reused_existing_build");
    push_bool_line(&mut lines, result, "uninstalled_existing_package");
    push_string_line(&mut lines, result, "serial");

    if let Some(manifest) = result.get("manifest") {
        for field in [
            "repository",
            "run_id",
            "artifact_name",
            "checkout_ref",
            "commit_sha",
            "version_name",
            "package_name",
            "activity_name",
            "android_validation_mode",
            "interactive_debug_profile",
        ] {
            push_string_line(&mut lines, manifest, field);
        }
    }

    if let Some(satisfied) = result
        .get("postcondition")
        .and_then(|postcondition| postcondition.get("satisfied"))
        .and_then(Value::as_bool)
    {
        lines.push(format!("postcondition_satisfied: {satisfied}"));
    }

    lines.join("\n")
}

fn push_string_line(lines: &mut Vec<String>, value: &Value, field: &str) {
    if let Some(text) = value.get(field).and_then(Value::as_str)
        && !text.is_empty()
    {
        lines.push(format!("{field}: {}", compact_summary_text(text)));
    }
}

fn push_bool_line(lines: &mut Vec<String>, value: &Value, field: &str) {
    if let Some(flag) = value.get(field).and_then(Value::as_bool) {
        lines.push(format!("{field}: {flag}"));
    }
}

fn compact_summary_text(text: &str) -> String {
    const LIMIT: usize = 96;
    if text.chars().count() <= LIMIT {
        return text.to_string();
    }
    let mut compact = text.chars().take(LIMIT - 3).collect::<String>();
    compact.push_str("...");
    compact
}

fn append_text(items: &mut [ComputerUseCallOutputContentItem], extra: &str) {
    if let Some(ComputerUseCallOutputContentItem::InputText { text }) = items.first_mut() {
        text.push_str(extra);
    }
}

fn prepend_text(items: &mut Vec<ComputerUseCallOutputContentItem>, prefix: &str) {
    if let Some(ComputerUseCallOutputContentItem::InputText { text }) = items.first_mut() {
        *text = format!("{prefix}\n\n{text}");
    } else {
        items.insert(
            0,
            ComputerUseCallOutputContentItem::InputText {
                text: prefix.to_string(),
            },
        );
    }
}

fn action_failure_summary(
    failed_action_kind: &str,
    action_error: &str,
    completed_summaries: &[String],
) -> String {
    let mut lines = vec![format!(
        "Android action `{failed_action_kind}` failed: {}",
        compact_summary_text(action_error)
    )];
    if !completed_summaries.is_empty() {
        lines.push("Actions completed before failure:".to_string());
        lines.extend(
            completed_summaries
                .iter()
                .map(|summary| format!("- {summary}")),
        );
    }
    lines.join("\n")
}

fn observation_labels(observation: &Value) -> Vec<String> {
    if let Some(labels) = visible_ui_labels(observation)
        && !labels.is_empty()
    {
        return labels;
    }

    observation
        .get("nodes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|node| {
            let text = first_string(
                node,
                &["text", "contentDescription", "resourceId", "className"],
            )?;
            let bounds = node.get("bounds").map(compact_json);
            Some(match bounds {
                Some(bounds) => format!("{text} {bounds}"),
                None => text,
            })
        })
        .take(24)
        .collect()
}

fn visible_ui_labels(observation: &Value) -> Option<Vec<String>> {
    Some(
        observation
            .get("visible_ui")?
            .get("nodes")?
            .as_array()?
            .iter()
            .filter_map(|node| {
                let text = first_string(
                    node,
                    &[
                        "label",
                        "text",
                        "contentDescription",
                        "resourceId",
                        "className",
                    ],
                )?;
                let text = visible_ui_label_with_state(text, node);
                let bounds = node.get("bounds").map(compact_json);
                Some(match bounds {
                    Some(bounds) => format!("{text} {bounds}"),
                    None => text,
                })
            })
            .take(24)
            .collect(),
    )
}

fn visible_ui_label_with_state(text: String, node: &Value) -> String {
    let lower_text = text.to_ascii_lowercase();
    let mut tags = Vec::new();

    if node.get("enabled").and_then(Value::as_bool) == Some(false)
        && !lower_text.contains("[disabled]")
    {
        tags.push("disabled".to_string());
    }
    if node.get("scrollable").and_then(Value::as_bool) == Some(true)
        && !lower_text.contains("[scrollable]")
    {
        tags.push("scrollable".to_string());
    }
    if node.get("clipped").and_then(Value::as_bool) == Some(true)
        && !lower_text.contains("[clipped")
    {
        let mut clipped = "clipped".to_string();
        if let Some(edges) = first_string_array(node, &["clip_edges", "clipEdges"])
            && !edges.is_empty()
        {
            clipped.push(' ');
            clipped.push_str(&edges.join("/"));
        }
        if let Some(percent) = first_u64(
            node,
            &["visible_fraction_percent", "visibleFractionPercent"],
        ) && percent < 100
        {
            clipped.push_str(&format!(" {percent}%"));
        }
        tags.push(clipped);
    }

    if tags.is_empty() {
        text
    } else {
        format!("{text} [{}]", tags.join("; "))
    }
}

fn screenshot_path(value: &Value) -> Option<&str> {
    value
        .get("artifacts")
        .and_then(|artifacts| artifacts.get("screenshot_path"))
        .or_else(|| value.get("path"))
        .and_then(Value::as_str)
}

fn artifact_bytes(value: &Value) -> Result<Vec<u8>, String> {
    let encoded = value
        .get("base64")
        .or_else(|| value.get("data_base64"))
        .or_else(|| value.get("content_base64"))
        .and_then(Value::as_str)
        .ok_or_else(|| "artifact response did not include base64 content".to_string())?;
    BASE64_STANDARD
        .decode(encoded)
        .map_err(|err| format!("invalid artifact base64: {err}"))
}

fn tool_result(mut value: Value) -> AndroidToolResult {
    let content = value
        .get_mut("content")
        .map(Value::take)
        .and_then(|value| match value {
            Value::Array(content) => Some(content),
            _ => None,
        })
        .unwrap_or_default();

    if let Some(structured) = value.get_mut("structuredContent") {
        return AndroidToolResult::new(structured.take(), content);
    }

    for item in &content {
        if let Some(text) = item.get("text").and_then(Value::as_str)
            && let Ok(parsed) = serde_json::from_str(text)
        {
            return AndroidToolResult::new(parsed, content);
        }
    }
    AndroidToolResult::new(value, content)
}

fn append_mcp_image_content(
    items: &mut Vec<ComputerUseCallOutputContentItem>,
    content: Vec<Value>,
) {
    for item in content {
        if let Some(image_item) = mcp_image_content_item(item) {
            items.push(image_item);
        }
    }
}

fn mcp_image_content_item(mut value: Value) -> Option<ComputerUseCallOutputContentItem> {
    if value.get("type").and_then(Value::as_str)? != "image" {
        return None;
    }
    let detail = mcp_image_detail(&value).or_else(|| Some("high".to_string()));
    let data = value.get_mut("data")?.take();
    let data = match data {
        Value::String(data) => data,
        _ => return None,
    };
    if data.trim().is_empty() {
        return None;
    }
    let image_url = if data.starts_with("data:") {
        data
    } else {
        let mime_type = value
            .get("mimeType")
            .or_else(|| value.get("mime_type"))
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream");
        format!("data:{mime_type};base64,{data}")
    };
    Some(ComputerUseCallOutputContentItem::InputImage { image_url, detail })
}

fn mcp_image_detail(value: &Value) -> Option<String> {
    let detail = value
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|meta| meta.get("codex/imageDetail"))
        .and_then(Value::as_str)?;
    match detail {
        "auto" | "low" | "high" | "original" => Some(detail.to_string()),
        _ => None,
    }
}

fn items_include_native_image(items: &[ComputerUseCallOutputContentItem]) -> bool {
    items
        .iter()
        .any(|item| matches!(item, ComputerUseCallOutputContentItem::InputImage { .. }))
}

fn tool_text(value: &Value) -> Option<String> {
    value
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .next()
        .map(ToString::to_string)
}

fn parse_event_stream_json(text: &str) -> Result<Value, String> {
    let mut json_rpc_response = None;
    let mut final_json = None;
    let mut event_data = Vec::new();

    fn finish_event(
        event_data: &mut Vec<String>,
        json_rpc_response: &mut Option<Value>,
        final_json: &mut Option<Value>,
    ) {
        if event_data.is_empty() {
            return;
        }

        let payload = event_data.join("\n");
        event_data.clear();

        let trimmed = payload.trim();
        if trimmed.is_empty() || trimmed == "[DONE]" {
            return;
        }

        if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
            if value.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
                && (value.get("result").is_some() || value.get("error").is_some())
            {
                *json_rpc_response = Some(value.clone());
            }
            *final_json = Some(value);
        }
    }

    for line in text.lines() {
        if line.trim().is_empty() {
            finish_event(&mut event_data, &mut json_rpc_response, &mut final_json);
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            event_data.push(rest.trim_start().to_string());
        }
    }

    finish_event(&mut event_data, &mut json_rpc_response, &mut final_json);

    let Some(value) = json_rpc_response.or(final_json) else {
        return Err("Android provider event stream omitted data payload".to_string());
    };
    Ok(value)
}

fn failed_response(error: String) -> ComputerUseCallResponse {
    let retryability = provider_unavailable_retryability(&error);
    let text = match retryability {
        Some(retryability) => {
            format!("Android provider unavailable\nretryability: {retryability}\nreason: {error}")
        }
        None => error.clone(),
    };
    let response_error = if retryability.is_some() {
        format!("Android provider unavailable: {error}")
    } else {
        error
    };
    ComputerUseCallResponse {
        content_items: vec![ComputerUseCallOutputContentItem::InputText { text }],
        success: false,
        error: Some(response_error),
    }
}

fn provider_unavailable_retryability(error: &str) -> Option<&'static str> {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("android provider http 530")
        || normalized.contains("error code: 1033")
        || normalized.contains("cloudflare tunnel")
        || normalized.contains("tunnel error")
        || normalized.contains("failed to reach android provider")
        || normalized.contains("connection refused")
        || normalized.contains("connection reset")
        || normalized.contains("timed out")
        || normalized.contains("temporary failure")
    {
        Some("retry_same_request")
    } else {
        None
    }
}

fn launch_app_args(action: &Value, defaults: &AndroidProviderDefaults) -> Result<Value, String> {
    let package_name = first_string(action, &["package_name", "package"])
        .or_else(|| defaults.package_name.clone())
        .ok_or_else(|| {
            "launch_app requires package_name/package or configured default_package_name."
                .to_string()
        })?;

    let mut args = json!({
        "package_name": package_name,
    });
    if let Some(activity) = first_string(action, &["activity"]).or_else(|| {
        (defaults.package_name.as_ref() == Some(&package_name))
            .then(|| defaults.activity.clone())
            .flatten()
    }) {
        args["activity"] = Value::String(activity);
    }
    copy_serial_or_default(action, &mut args, defaults);
    copy_if_present(action, &mut args, "wait_for_activity");
    copy_if_present(action, &mut args, "wait_for_package");
    copy_if_present(action, &mut args, "wait_for_selector");
    copy_if_present(action, &mut args, "timeout_secs");
    Ok(args)
}

fn input_args(action: &Value, fields: &[&str], defaults: &AndroidProviderDefaults) -> Value {
    let mut args = json!({});
    for field in fields {
        copy_if_present(action, &mut args, field);
    }
    copy_serial_or_default(action, &mut args, defaults);
    copy_if_present(action, &mut args, "expect_scroll_change");
    copy_if_present(action, &mut args, "wait_for_activity");
    copy_if_present(action, &mut args, "wait_for_package");
    copy_if_present(action, &mut args, "wait_for_selector");
    copy_if_present(action, &mut args, "timeout_secs");
    args
}

fn multi_touch_args(
    action: &Value,
    defaults: &AndroidProviderDefaults,
) -> Result<Value, String> {
    let pointers = action
        .get("pointers")
        .and_then(Value::as_array)
        .ok_or_else(|| "multi_touch requires a pointers array.".to_string())?;
    if !(2..=5).contains(&pointers.len()) {
        return Err("multi_touch requires between two and five pointers.".to_string());
    }

    for (index, pointer) in pointers.iter().enumerate() {
        let pointer = pointer
            .as_object()
            .ok_or_else(|| format!("multi_touch pointer {index} must be an object."))?;
        for coordinate in ["x1", "y1", "x2", "y2"] {
            let value = pointer.get(coordinate).and_then(Value::as_u64).ok_or_else(|| {
                format!(
                    "multi_touch pointer {index} requires non-negative integer {coordinate}."
                )
            })?;
            if value > u64::from(u32::MAX) {
                return Err(format!(
                    "multi_touch pointer {index} coordinate {coordinate} exceeds the supported range."
                ));
            }
        }
    }

    let mut args = json!({
        "pointers": Value::Array(pointers.clone()),
    });
    if let Some(duration) = action.get("duration_ms") {
        let duration = duration.as_u64().ok_or_else(|| {
            "multi_touch duration_ms must be an integer from 50 through 2000.".to_string()
        })?;
        if !(50..=2000).contains(&duration) {
            return Err(
                "multi_touch duration_ms must be an integer from 50 through 2000.".to_string(),
            );
        }
        args["duration_ms"] = Value::from(duration);
    }
    copy_serial_or_default(action, &mut args, defaults);
    copy_if_present(action, &mut args, "timeout_secs");
    Ok(args)
}

fn element_args(action: &Value, defaults: &AndroidProviderDefaults) -> Result<Value, String> {
    let mut args = json!({});
    if let Some(selector) = action.get("selector").or_else(|| action.get("target")) {
        args["selector"] = selector.clone();
    } else {
        return Err("element action requires selector or target.".to_string());
    }
    copy_serial_or_default(action, &mut args, defaults);
    copy_if_present(action, &mut args, "match_index");
    copy_if_present(action, &mut args, "wait_for_selector");
    copy_if_present(action, &mut args, "wait_until_absent");
    copy_if_present(action, &mut args, "timeout_secs");
    Ok(args)
}

fn scroll_args(action: &Value, defaults: &AndroidProviderDefaults) -> Result<Value, String> {
    if ["x1", "y1", "x2", "y2"]
        .iter()
        .all(|field| action.get(field).is_some())
    {
        return Ok(input_args(
            action,
            &["x1", "y1", "x2", "y2", "duration_ms"],
            defaults,
        ));
    }
    let scroll_y = action
        .get("scroll_y")
        .and_then(Value::as_i64)
        .ok_or_else(|| "scroll requires x1/y1/x2/y2 or scroll_y.".to_string())?;
    let x = action.get("x").and_then(Value::as_i64).unwrap_or(540);
    let y = action.get("y").and_then(Value::as_i64).unwrap_or(1200);
    let mut args = json!({
        "x1": x,
        "y1": y,
        "x2": x,
        "y2": y - scroll_y,
    });
    copy_if_present(action, &mut args, "duration_ms");
    copy_serial_or_default(action, &mut args, defaults);
    Ok(args)
}

fn arguments_with_default_serial(arguments: &Value, defaults: &AndroidProviderDefaults) -> Value {
    let mut args = arguments.clone();
    copy_serial_or_default(arguments, &mut args, defaults);
    args
}

fn copy_serial_or_default(
    source: &Value,
    target: &mut Value,
    defaults: &AndroidProviderDefaults,
) -> bool {
    if copy_if_present(source, target, "serial") {
        true
    } else if let Some(serial) = &defaults.serial {
        target["serial"] = Value::String(serial.clone());
        true
    } else {
        false
    }
}

fn copy_first_present(source: &Value, target: &mut Value, fields: &[&str]) {
    for field in fields {
        if copy_if_present(source, target, field) {
            return;
        }
    }
}

fn copy_if_present(source: &Value, target: &mut Value, field: &str) -> bool {
    if let Some(value) = source.get(field) {
        target[field] = value.clone();
        true
    } else {
        false
    }
}

fn copy_inspect_screenshot_filename_for_capture(source: &Value, target: &mut Value) {
    if let Some(value) = source.get("screenshot_filename") {
        target["filename"] = value.clone();
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
    if response_includes_native_image(response) {
        return;
    }

    append_text(
        &mut response.content_items,
        &format!(
            "\n\n{missing_image_message} The provider must return screenshots as native image content items rather than text-only summaries or artifact paths."
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

fn has_xy(value: &Value) -> bool {
    value.get("x").is_some() && value.get("y").is_some()
}

fn first_string(value: &Value, fields: &[&str]) -> Option<String> {
    fields
        .iter()
        .filter_map(|field| value.get(field).and_then(Value::as_str))
        .find(|text| !text.is_empty())
        .map(ToString::to_string)
}

fn first_string_array(value: &Value, fields: &[&str]) -> Option<Vec<String>> {
    fields
        .iter()
        .find_map(|field| value.get(field).and_then(Value::as_array))
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToString::to_string)
                .collect()
        })
}

fn first_u64(value: &Value, fields: &[&str]) -> Option<u64> {
    fields
        .iter()
        .find_map(|field| value.get(field).and_then(Value::as_u64))
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".to_string())
}

fn value_display(value: Option<&Value>) -> String {
    value
        .map(compact_json)
        .unwrap_or_else(|| "<missing>".to_string())
}

fn first_env(keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.trim().is_empty())
}

fn format_http_error(status: StatusCode, text: &str) -> String {
    let snippet: String = text.chars().take(500).collect();
    format!("Android provider HTTP {status}: {snippet}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_actions_prefers_actions_array() {
        let actions = canonical_actions(&json!({
            "type": "tap",
            "actions": [
                {"type": "wait", "ms": 10},
                {"type": "tap", "x": 1, "y": 2}
            ]
        }));
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0]["type"], "wait");
    }

    #[test]
    fn parse_event_stream_json_reads_data_payload() {
        let parsed = parse_event_stream_json(
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true}}\n\n",
        )
        .expect("event stream should parse");
        assert_eq!(parsed["result"]["ok"], true);
    }

    #[test]
    fn parse_event_stream_json_uses_final_json_event() {
        let parsed = parse_event_stream_json(
            "event: progress\ndata: {\"progress\":0.5}\n\n\
             event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true}}\n\n",
        )
        .expect("event stream should parse final JSON-RPC event");
        assert_eq!(parsed["result"]["ok"], true);
    }

    #[test]
    fn parse_event_stream_json_prefers_json_rpc_response_over_later_done_event() {
        let parsed = parse_event_stream_json(
            "event: progress\ndata: {\"progress\":0.5}\n\n\
             event: message\ndata: {\"jsonrpc\":\"2.0\",\"result\":{\"ok\":true}}\n\n\
             event: done\ndata: {\"done\":true}\n\n",
        )
        .expect("event stream should retain final JSON-RPC response");
        assert_eq!(parsed["result"]["ok"], true);
        assert_eq!(parsed.get("done"), None);
    }

    #[test]
    fn parse_event_stream_json_joins_multiline_data_event() {
        let parsed = parse_event_stream_json(
            "event: message\n\
             data: {\"jsonrpc\":\"2.0\",\n\
             data: \"result\":{\"ok\":true}}\n\n",
        )
        .expect("event stream should parse multiline event data");
        assert_eq!(parsed["result"]["ok"], true);
    }

    #[test]
    fn summarize_observation_keeps_artifact_paths_internal() {
        let summary = summarize_observation(
            "Android observation",
            &json!({
                "serial": "emulator-5554",
                "node_count": 2,
                "current_focus": {"package": "com.example"},
                "artifacts": {"screenshot_path": "/tmp/screen.png"},
                "nodes": [
                    {"text": "Launch", "bounds": {"left": 1, "top": 2}},
                    {"contentDescription": "Settings"}
                ]
            }),
        );
        assert!(summary.contains("serial: emulator-5554"));
        assert!(summary.contains("Launch"));
        assert!(!summary.contains("screenshot_artifact"));
        assert!(!summary.contains("/tmp/screen.png"));
    }

    #[test]
    fn summarize_observation_prefers_visible_ui_digest_with_state() {
        let summary = summarize_observation(
            "Android observation",
            &json!({
                "serial": "emulator-5554",
                "node_count": 4,
                "nodes": [
                    {"text": "Raw Frame", "bounds": {"left": 1, "top": 2}},
                ],
                "visible_ui": {
                    "nodes": [
                        {
                            "label": "Frame",
                            "bounds": {"left": 48, "top": 96, "right": 240, "bottom": 180},
                            "enabled": false
                        },
                        {
                            "label": "Mission feed",
                            "bounds": {"left": 48, "top": 220, "right": 720, "bottom": 420},
                            "scrollable": true
                        },
                        {
                            "label": "Advance",
                            "bounds": {"left": 1040, "top": 300, "right": 1120, "bottom": 360},
                            "clipped": true,
                            "clip_edges": ["right"],
                            "visible_fraction_percent": 50
                        }
                    ]
                }
            }),
        );

        assert!(summary.contains("Frame [disabled]"));
        assert!(summary.contains("Mission feed [scrollable]"));
        assert!(summary.contains("Advance [clipped right 50%]"));
        assert!(!summary.contains("Raw Frame"));
    }

    #[test]
    fn install_build_from_run_gets_extended_provider_timeout() {
        assert_eq!(
            request_timeout_for_tool(ANDROID_OBSERVE_TOOL_NAME),
            DEFAULT_REQUEST_TIMEOUT
        );
        assert_eq!(
            request_timeout_for_tool(ANDROID_STEP_TOOL_NAME),
            DEFAULT_REQUEST_TIMEOUT
        );
        assert_eq!(
            request_timeout_for_tool(ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME),
            INSTALL_REQUEST_TIMEOUT
        );
    }

    #[test]
    fn configured_android_tools_load_from_explicit_codex_home() {
        let codex_home = tempfile::tempdir().expect("create temp codex home");
        std::fs::write(
            codex_home.path().join("android-computer-use.json"),
            r#"{"mcp_url":"https://android-provider.example/mcp"}"#,
        )
        .expect("write android provider config");

        let tools = configured_android_dynamic_tools_for_codex_home(codex_home.path());

        assert_eq!(
            tools.into_iter().map(|tool| tool.name).collect::<Vec<_>>(),
            vec![
                ANDROID_OBSERVE_TOOL_NAME.to_string(),
                ANDROID_STEP_TOOL_NAME.to_string(),
                ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME.to_string(),
            ]
        );
    }

    #[test]
    fn android_config_loads_provider_defaults_from_config_file() {
        let codex_home = tempfile::tempdir().expect("create temp codex home");
        std::fs::write(
            codex_home
                .path()
                .join("solarlab-android-dynamic-tools.json"),
            r#"{
                "mcp_url":"https://android-provider.example/mcp",
                "default_serial":"emulator-5554",
                "default_package_name":"com.example.app",
                "default_activity":".MainActivity"
            }"#,
        )
        .expect("write android provider config");

        let config = AndroidRuntimeConfig::load_with_env(codex_home.path(), |_| None)
            .expect("android provider config should load");

        assert_eq!(config.mcp_url, "https://android-provider.example/mcp");
        assert_eq!(
            config.defaults,
            AndroidProviderDefaults {
                serial: Some("emulator-5554".to_string()),
                package_name: Some("com.example.app".to_string()),
                activity: Some(".MainActivity".to_string()),
            }
        );
    }

    #[test]
    fn android_config_ignores_blank_provider_defaults_from_config_file() {
        let codex_home = tempfile::tempdir().expect("create temp codex home");
        std::fs::write(
            codex_home.path().join("android-computer-use.json"),
            r#"{
                "mcp_url":"https://android-provider.example/mcp",
                "default_serial":" ",
                "default_package_name":"",
                "default_activity":"\t"
            }"#,
        )
        .expect("write android provider config");

        let config = AndroidRuntimeConfig::load_with_env(codex_home.path(), |_| None)
            .expect("android provider config should load");

        assert_eq!(config.mcp_url, "https://android-provider.example/mcp");
        assert_eq!(config.defaults, AndroidProviderDefaults::default());
    }

    #[test]
    fn prefer_stable_ui_defaults_to_true_unless_disabled() {
        assert!(prefer_stable_ui(&json!({})));
        assert!(prefer_stable_ui(&json!({"stable": true})));
        assert!(prefer_stable_ui(&json!({"wait_for_stable_ui": true})));
        assert!(!prefer_stable_ui(&json!({"stable": false})));
        assert!(!prefer_stable_ui(&json!({"wait_for_stable_ui": false})));
    }

    #[test]
    fn summarize_install_result_keeps_large_provider_payloads_out_of_transcript() {
        let summary = summarize_install_result(&json!({
            "ok": true,
            "installed": true,
            "serial": "emulator-5554",
            "apk_path": "/tmp/local-build-cache/app.apk",
            "install_stdout": "very noisy adb stdout",
            "manifest": {
                "repository": "sednalabs/solar-gravity-lab",
                "run_id": "25106447821",
                "artifact_name": "interactive-android-build-stage-first-mirror-on-hosted-debug-lite",
                "checkout_ref": "feature-branch",
                "commit_sha": "acedb057b55387fe121fa82ca2e4af67d98741d0",
                "version_name": "0.1.1-alpha.2",
                "package_name": "com.sednalabs.solarlab",
                "activity_name": ".MainActivity",
                "android_validation_mode": "stage-first-mirror-on",
                "interactive_debug_profile": "hosted-debug-lite"
            },
            "postcondition": {
                "satisfied": true
            }
        }));

        assert!(summary.contains("Android build install"));
        assert!(summary.contains("installed: true"));
        assert!(summary.contains("run_id: 25106447821"));
        assert!(summary.contains("postcondition_satisfied: true"));
        assert!(!summary.contains("apk_path"));
        assert!(!summary.contains("install_stdout"));
        assert!(!summary.contains("/tmp/local-build-cache"));
    }

    #[test]
    fn artifact_bytes_decodes_known_shapes() {
        let bytes = artifact_bytes(&json!({"base64": "aGVsbG8="})).expect("decode base64");
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn tool_result_preserves_images_when_structured_content_exists() {
        let result = tool_result(json!({
            "structuredContent": {
                "ok": true,
                "artifacts": {"screenshot_path": "/tmp/screen.png"}
            },
            "content": [
                {"type": "text", "text": "summary"},
                {
                    "type": "image",
                    "data": "UE5H",
                    "mimeType": "image/png",
                    "_meta": {"codex/imageDetail": "original"}
                }
            ]
        }));

        assert_eq!(result.structured_content()["ok"], true);
        let mut items = Vec::new();
        append_mcp_image_content(&mut items, result.content);
        assert_eq!(
            items,
            vec![ComputerUseCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,UE5H".to_string(),
                detail: Some("original".to_string()),
            }]
        );
    }

    #[test]
    fn tool_result_parses_json_text_without_dropping_mcp_image_content() {
        let result = tool_result(json!({
            "content": [
                {"type": "text", "text": "{\"ok\":true}"},
                {"type": "image", "data": "data:image/png;base64,UE5H"}
            ]
        }));

        assert_eq!(result.structured_content()["ok"], true);
        let mut items = Vec::new();
        append_mcp_image_content(&mut items, result.content);
        assert_eq!(
            items,
            vec![ComputerUseCallOutputContentItem::InputImage {
                image_url: "data:image/png;base64,UE5H".to_string(),
                detail: Some("high".to_string()),
            }]
        );
    }

    #[test]
    fn response_includes_native_image_detects_image_content() {
        let response = ComputerUseCallResponse {
            content_items: vec![
                ComputerUseCallOutputContentItem::InputText {
                    text: "summary".to_string(),
                },
                ComputerUseCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAAA".to_string(),
                    detail: Some("high".to_string()),
                },
            ],
            success: true,
            error: None,
        };

        assert!(response_includes_native_image(&response));
    }

    #[test]
    fn visual_response_without_native_image_is_failed_loudly() {
        let mut response = ComputerUseCallResponse {
            content_items: vec![ComputerUseCallOutputContentItem::InputText {
                text: "Android observation\nvisible_ui: text only".to_string(),
            }],
            success: true,
            error: Some("android.inspect_ui failed".to_string()),
        };

        require_native_image_for_visual_response(
            &mut response,
            "Android observation missing native image output.",
        );

        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some(
                "Android observation missing native image output. Previous provider error: android.inspect_ui failed"
            )
        );
        let ComputerUseCallOutputContentItem::InputText { text } = &response.content_items[0]
        else {
            panic!("expected text summary");
        };
        assert!(text.contains("visible_ui: text only"));
        assert!(text.contains("must return screenshots as native image content items"));
    }

    #[test]
    fn visual_response_with_native_image_remains_successful() {
        let mut response = ComputerUseCallResponse {
            content_items: vec![
                ComputerUseCallOutputContentItem::InputText {
                    text: "Android observation".to_string(),
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
            "Android observation missing native image output.",
        );

        assert!(response.success);
        assert_eq!(response.error, None);
    }

    #[test]
    fn inspect_ui_retry_filter_accepts_transient_hierarchy_races() {
        assert!(should_retry_inspect_ui_error(
            "UI hierarchy capture was unavailable after atomic stream and legacy retry paths; retry observation"
        ));
        assert!(should_retry_inspect_ui_error(
            "adb: error: failed to stat remote object '/sdcard/window-dump.xml': No such file or directory"
        ));
        assert!(!should_retry_inspect_ui_error(
            "Android provider HTTP 403: forbidden"
        ));
    }

    #[test]
    fn launch_app_args_uses_configured_default_package_and_activity() {
        let defaults = AndroidProviderDefaults {
            serial: Some("emulator-5554".to_string()),
            package_name: Some("com.example.app".to_string()),
            activity: Some(".MainActivity".to_string()),
        };

        let args = launch_app_args(&json!({"type": "launch_app"}), &defaults)
            .expect("launch args should use configured defaults");

        assert_eq!(
            args,
            json!({
                "package_name": "com.example.app",
                "activity": ".MainActivity",
                "serial": "emulator-5554"
            })
        );
    }

    #[test]
    fn launch_app_args_does_not_leak_default_activity_to_other_package() {
        let defaults = AndroidProviderDefaults {
            serial: None,
            package_name: Some("com.example.app".to_string()),
            activity: Some(".MainActivity".to_string()),
        };

        let args = launch_app_args(
            &json!({"type": "launch_app", "package": "com.other.app"}),
            &defaults,
        )
        .expect("explicit package should build launch args");

        assert_eq!(args, json!({"package_name": "com.other.app"}));
    }

    #[test]
    fn input_args_applies_default_serial_without_clobbering_explicit_serial() {
        let defaults = AndroidProviderDefaults {
            serial: Some("emulator-5554".to_string()),
            ..AndroidProviderDefaults::default()
        };

        assert_eq!(
            input_args(&json!({"x": 1, "y": 2}), &["x", "y"], &defaults),
            json!({"x": 1, "y": 2, "serial": "emulator-5554"})
        );
        assert_eq!(
            input_args(
                &json!({"x": 1, "y": 2, "serial": "device-1"}),
                &["x", "y"],
                &defaults
            ),
            json!({"x": 1, "y": 2, "serial": "device-1"})
        );
    }

    #[test]
    fn multi_touch_args_preserves_the_atomic_provider_contract() {
        let defaults = AndroidProviderDefaults {
            serial: Some("emulator-5554".to_string()),
            ..AndroidProviderDefaults::default()
        };
        let args = multi_touch_args(
            &json!({
                "type": "multi_touch",
                "pointers": [
                    {"x1": 100, "y1": 200, "x2": 80, "y2": 180},
                    {"x1": 300, "y1": 200, "x2": 320, "y2": 180}
                ],
                "duration_ms": 240,
                "timeout_secs": 12
            }),
            &defaults,
        )
        .expect("valid multi-touch args");

        assert_eq!(
            args,
            json!({
                "pointers": [
                    {"x1": 100, "y1": 200, "x2": 80, "y2": 180},
                    {"x1": 300, "y1": 200, "x2": 320, "y2": 180}
                ],
                "duration_ms": 240,
                "serial": "emulator-5554",
                "timeout_secs": 12
            })
        );
    }

    #[test]
    fn multi_touch_args_rejects_invalid_pointer_shapes_and_duration() {
        let defaults = AndroidProviderDefaults::default();
        for (action, expected_error) in [
            (
                json!({"pointers": [{"x1": 1, "y1": 2, "x2": 3, "y2": 4}]}),
                "between two and five pointers",
            ),
            (
                json!({
                    "pointers": [
                        {"x1": 1, "y1": 2, "x2": 3, "y2": 4},
                        {"x1": -1, "y1": 2, "x2": 3, "y2": 4}
                    ]
                }),
                "non-negative integer x1",
            ),
            (
                json!({
                    "pointers": [
                        {"x1": 1, "y1": 2, "x2": 3, "y2": 4},
                        {"x1": 5, "y1": 6, "x2": 7, "y2": 8}
                    ],
                    "duration_ms": 49
                }),
                "from 50 through 2000",
            ),
        ] {
            let error = multi_touch_args(&action, &defaults).expect_err("invalid multi-touch");
            assert!(
                error.contains(expected_error),
                "expected {expected_error:?} in {error:?}"
            );
        }
    }

    #[test]
    fn install_args_applies_default_serial() {
        let defaults = AndroidProviderDefaults {
            serial: Some("emulator-5554".to_string()),
            ..AndroidProviderDefaults::default()
        };

        assert_eq!(
            arguments_with_default_serial(&json!({"workflow_run_id": 123}), &defaults),
            json!({"workflow_run_id": 123, "serial": "emulator-5554"})
        );
    }

    #[test]
    fn failed_response_classifies_transient_provider_unavailability() {
        let response = failed_response("Android provider HTTP 530: error code: 1033".to_string());

        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("Android provider unavailable: Android provider HTTP 530: error code: 1033")
        );
        let ComputerUseCallOutputContentItem::InputText { text } = &response.content_items[0]
        else {
            panic!("expected text response");
        };
        assert!(text.contains("Android provider unavailable"));
        assert!(text.contains("retryability: retry_same_request"));
    }

    #[test]
    fn scroll_args_maps_scroll_delta_to_swipe() {
        let args = scroll_args(
            &json!({"type": "scroll", "scroll_y": 300, "x": 500, "y": 1000}),
            &AndroidProviderDefaults::default(),
        )
        .expect("scroll args");
        assert_eq!(args["x1"], 500);
        assert_eq!(args["y1"], 1000);
        assert_eq!(args["x2"], 500);
        assert_eq!(args["y2"], 700);
    }

    #[test]
    fn action_kind_accepts_current_and_compatibility_fields() {
        assert_eq!(action_kind(&json!({"type": "tap"})), "tap");
        assert_eq!(action_kind(&json!({"action": "swipe"})), "swipe");
        assert_eq!(
            action_kind(&json!({"name": "semantic_action"})),
            "semantic_action"
        );
        assert_eq!(action_kind(&json!({})), "unknown");
    }

    #[test]
    fn action_failure_summary_lists_completed_actions_without_payload_echo() {
        let summary = action_failure_summary(
            "tap",
            "selector timed out",
            &[
                "launched Android app".to_string(),
                "typed Android text".to_string(),
            ],
        );

        assert_eq!(
            summary,
            "Android action `tap` failed: selector timed out\n\
             Actions completed before failure:\n\
             - launched Android app\n\
             - typed Android text"
        );
        assert!(!summary.contains("secret"));
    }
}
