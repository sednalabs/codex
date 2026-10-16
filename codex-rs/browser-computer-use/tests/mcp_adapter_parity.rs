use anyhow::Result;
use codex_android_computer_use::AndroidComputerUseOutcome;
use codex_android_computer_use::handle_android_computer_use_for_codex_home;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::CodeModeNestedToolCall;
use codex_code_mode_protocol::CodeModeSessionDelegate;
use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::NotificationFuture;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::ToolDefinition;
use codex_code_mode_protocol::ToolInvocationFuture;
use codex_code_mode_runtime::InProcessCodeModeSession;
use codex_core_plugins::ExecutorPluginProvider;
use codex_core_plugins::PluginCatalog;
use codex_core_plugins::PluginCatalogEntry;
use codex_core_plugins::PluginIdentity;
use codex_core_plugins::PluginListQuery;
use codex_core_plugins::PluginProvider;
use codex_core_plugins::PluginProviderFuture;
use codex_core_plugins::PluginSourceLocation;
use codex_exec_server::EnvironmentManager;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_mcp_extension::PluginProviders;
use codex_mcp_extension::install_plugin_providers;
use codex_protocol::ToolName;
use core_test_support::apps_test_server::AppsTestServer;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::run_test_with_large_stack;
use core_test_support::test_codex::test_codex;
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio_util::sync::CancellationToken;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::method;

#[path = "support/curated_mcp_fixture.rs"]
mod support;

struct SyntheticPluginCatalog {
    catalog: PluginCatalog,
    calls: AtomicUsize,
}

impl PluginProvider for SyntheticPluginCatalog {
    fn list(&self, _query: PluginListQuery) -> PluginProviderFuture<'_, PluginCatalog> {
        Box::pin(async {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.catalog.clone())
        })
    }
}

#[test]
fn configured_plugin_mcp_image_reaches_the_model_as_typed_input_image() -> Result<()> {
    run_test_with_large_stack("browser-mcp-image-parity", run_image_parity)
}

#[tokio::test]
async fn code_mode_can_forward_a_curated_mcp_image_block_as_typed_content() -> Result<()> {
    let service = InProcessCodeModeSession::new();
    let response = service
        .execute(
            ExecuteRequest {
                tool_call_id: "code-mode-call".to_string(),
                enabled_tools: vec![ToolDefinition {
                    name: "observe".to_string(),
                    tool_name: ToolName::plain("observe"),
                    description: "Return synthetic MCP visual evidence.".to_string(),
                    kind: CodeModeToolKind::Function,
                    input_schema: None,
                    input_schema_max_bytes: None,
                    output_schema: None,
                }],
                source: r#"
const result = await tools.observe({});
for (const block of result.content) {
  if (block.type === "image") image(block);
}
"#
                .to_string(),
                yield_time_ms: None,
                max_output_tokens: None,
            },
            Arc::new(SyntheticImageDelegate),
            /*preempt*/ None,
        )
        .await?
        .initial_response()
        .await?;

    let RuntimeResponse::Result { content_items, .. } = response else {
        anyhow::bail!("Code Mode did not complete with a result");
    };
    let image_urls = content_items
        .iter()
        .filter_map(|item| match item {
            FunctionCallOutputContentItem::InputImage { image_url, .. } => Some(image_url.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(
        image_urls.as_slice() == [support::IMAGE_DATA_URL],
        "Code Mode did not preserve the MCP image as typed image content"
    );
    Ok(())
}

#[tokio::test]
async fn mock_android_operations_remain_ordered_and_forward_post_action_image() -> Result<()> {
    for variable in [
        "CODEX_ANDROID_MCP_URL",
        "SOLARLAB_ANDROID_MCP_URL",
        "CODEX_ANDROID_MCP_HOSTNAME",
        "SOLARLAB_ANDROID_MCP_HOSTNAME",
        "CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_ID",
        "CODEX_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET",
        "SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_ID",
        "SOLARLAB_ANDROID_MCP_CF_ACCESS_CLIENT_SECRET",
    ] {
        anyhow::ensure!(
            std::env::var_os(variable).is_none(),
            "mock Android test requires host Android endpoint overrides to be unset"
        );
    }

    let server = start_mock_server().await;
    let recorded = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let recorded_for_mock = Arc::clone(&recorded);
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .unwrap_or_else(|_| panic!("invalid synthetic Android request JSON"));
            recorded_for_mock
                .lock()
                .unwrap_or_else(|_| panic!("synthetic Android request recorder poisoned"))
                .push(body.clone());
            let id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let result = match body["method"].as_str().unwrap_or_default() {
                "initialize" => json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "serverInfo": {"name": "synthetic-android", "version": "1"}
                }),
                "tools/list" => json!({
                    "tools": [
                        {"name": "android.inspect_ui"},
                        {"name": "android.input.tap"},
                        {"name": "interactive_session.install_build_from_run"}
                    ]
                }),
                "tools/call" => match body["params"]["name"].as_str().unwrap_or_default() {
                    "android.inspect_ui" => json!({
                        "structuredContent": {"node_count": 0, "nodes": []},
            "content": [
                {"type": "text", "text": "synthetic visual result"},
                {"type": "image", "data": support::IMAGE_BASE64, "mimeType": "image/png"}
            ]
                    }),
                    "android.input.tap" => json!({
                        "structuredContent": {},
                        "content": [{"type": "text", "text": "synthetic tap accepted"}]
                    }),
                    "interactive_session.install_build_from_run" => json!({
                        "structuredContent": {"status": "installed"},
                        "content": [{"type": "text", "text": "synthetic install accepted"}]
                    }),
                    _ => json!({"isError": true, "content": []}),
                },
                _ => json!({}),
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            }))
        })
        .mount(&server)
        .await;

    let codex_home = tempfile::tempdir()?;
    std::fs::write(
        codex_home.path().join("android-computer-use.json"),
        serde_json::to_vec(&json!({"mcp_url": server.uri()}))?,
    )?;

    let step = handle_android_computer_use_for_codex_home(
        &DynamicToolCallParams {
            thread_id: "synthetic-thread".to_string(),
            turn_id: "synthetic-turn".to_string(),
            call_id: "synthetic-step".to_string(),
            namespace: Some("codex_android".to_string()),
            tool: "android_step".to_string(),
            arguments: json!({"type":"tap", "x":5, "y":7}),
        },
        codex_home.path(),
    )
    .await;
    let AndroidComputerUseOutcome::Handled(step) = step else {
        anyhow::bail!("mock Android step was not handled");
    };
    anyhow::ensure!(step.success, "mock Android step failed");
    anyhow::ensure!(
        step.content_items.iter().any(|item| matches!(
            item,
            DynamicToolCallOutputContentItem::InputImage { image_url }
                if image_url.as_str() == support::IMAGE_DATA_URL
        )),
        "mock Android post-action observation did not preserve typed image output"
    );

    let step_calls = recorded
        .lock()
        .map_err(|_| anyhow::anyhow!("synthetic Android request recorder poisoned"))?
        .iter()
        .filter(|body| body["method"] == "tools/call")
        .filter_map(|body| body["params"]["name"].as_str())
        .map(str::to_string)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        step_calls
            == vec![
                "android.input.tap".to_string(),
                "android.inspect_ui".to_string()
            ],
        "mock Android action and post-action observation order changed"
    );

    recorded
        .lock()
        .map_err(|_| anyhow::anyhow!("synthetic Android request recorder poisoned"))?
        .clear();
    let install = handle_android_computer_use_for_codex_home(
        &DynamicToolCallParams {
            thread_id: "synthetic-thread".to_string(),
            turn_id: "synthetic-turn".to_string(),
            call_id: "synthetic-install".to_string(),
            namespace: Some("codex_android".to_string()),
            tool: "android_install_build_from_run".to_string(),
            arguments: json!({"workflow_run_id":7, "artifact_name":"synthetic"}),
        },
        codex_home.path(),
    )
    .await;
    let AndroidComputerUseOutcome::Handled(install) = install else {
        anyhow::bail!("mock Android install was not handled");
    };
    anyhow::ensure!(install.success, "mock Android install failed");
    anyhow::ensure!(
        install.content_items.iter().any(|item| matches!(
            item,
            DynamicToolCallOutputContentItem::InputImage { image_url }
                if image_url.as_str() == support::IMAGE_DATA_URL
        )),
        "mock Android post-install observation did not preserve typed image output"
    );
    let install_calls = recorded
        .lock()
        .map_err(|_| anyhow::anyhow!("synthetic Android request recorder poisoned"))?
        .iter()
        .filter(|body| body["method"] == "tools/call")
        .filter_map(|body| body["params"]["name"].as_str())
        .map(str::to_string)
        .collect::<Vec<_>>();
    anyhow::ensure!(
        install_calls
            == vec![
                "interactive_session.install_build_from_run".to_string(),
                "android.inspect_ui".to_string()
            ],
        "mock Android install and post-install observation order changed"
    );
    Ok(())
}

struct SyntheticImageDelegate;

impl CodeModeSessionDelegate for SyntheticImageDelegate {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        _cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            if invocation.tool_name.name != "observe" {
                return Err("unexpected synthetic nested tool".to_string());
            }
            Ok(json!({
            "content": [
                {"type": "text", "text": "synthetic visual result"},
                {"type": "image", "data": support::IMAGE_BASE64, "mimeType": "image/png"}
            ]
            }))
        })
    }

    fn notify<'a>(
        &'a self,
        _call_id: String,
        _cell_id: CellId,
        _text: String,
        _cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async { Ok(()) })
    }

    fn cell_closed(&self, _cell_id: &CellId) {}
}

async fn run_image_parity() -> Result<()> {
    let model_server = start_mock_server().await;
    let apps = AppsTestServer::mount(&model_server).await?;
    let plugin_server = start_mock_server().await;
    let plugin_mcp = AppsTestServer::mount_with_tools(
        &plugin_server,
        Arc::new(std::sync::Mutex::new(vec![json!({
            "name": "observe",
            "description": "Return synthetic visual evidence.",
            "annotations": {"readOnlyHint": true},
            "inputSchema": {"type": "object", "properties": {}}
        })])),
    )
    .await?;

    Mock::given(method("POST"))
        .and(body_partial_json(json!({"method": "tools/call"})))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body)
                .unwrap_or_else(|_| panic!("invalid MCP tool request JSON"));
            ResponseTemplate::new(200)
                .set_body_json(support::image_tool_response(&body["id"]))
        })
        .with_priority(1)
        .mount(&plugin_server)
        .await;

    let declaration = json!({
        "url": format!("{}/api/codex/ps/mcp", plugin_mcp.chatgpt_base_url)
    });
    let catalog = PluginCatalog {
        entries: vec![PluginCatalogEntry {
            id: PluginIdentity::Remote {
                remote_plugin_id: "plugin_visual".into(),
            },
            display_name: "Synthetic Visual Evidence".into(),
            version: Some("1".into()),
            mcp_servers: [("visual".into(), declaration.to_string())].into(),
            connector_ids: Vec::new(),
            locations: vec![PluginSourceLocation::Cloud {
                resource_uri: "plugin://synthetic-visual".into(),
                bundle_uri: None,
            }],
        }],
        warnings: Vec::new(),
    };
    let cloud = Arc::new(SyntheticPluginCatalog {
        catalog,
        calls: AtomicUsize::new(0),
    });
    let mut extensions = ExtensionRegistryBuilder::new();
    let providers = PluginProviders::new(Arc::new(ExecutorPluginProvider::new(Arc::new(
        EnvironmentManager::default_for_tests(),
    ))))
    .with_cloud_provider(cloud.clone());
    install_plugin_providers(&mut extensions, providers);

    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(move |config| {
            assert!(config.features.enable(Feature::Plugins).is_ok());
            assert!(config.features.enable(Feature::Apps).is_ok());
            assert!(
                config
                    .features
                    .enable(Feature::ExecutorCapabilityDiscovery)
                    .is_ok()
            );
            config.chatgpt_base_url = apps.chatgpt_base_url;
        })
        .build(&model_server)
        .await?;

    let _response = mount_sse_sequence(
        &model_server,
        vec![
            sse(vec![
                ev_response_created("call"),
                ev_function_call_with_namespace(
                    "visual-call",
                    "mcp__visual",
                    "observe",
                    "{}",
                ),
                ev_completed("call"),
            ]),
            sse(vec![
                ev_assistant_message("done", "Received synthetic visual evidence."),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    test.submit_turn("Use the visual evidence plugin.").await?;

    let requests = _response.requests();
    anyhow::ensure!(requests.len() == 2, "expected one tool request and one follow-up");
    let output = requests[1].function_call_output("visual-call");
    let output_items = output["output"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("expected structured function-call output"))?;
    let model_images = output_items
        .iter()
        .filter(|item| item["type"] == "input_image")
        .filter_map(|item| item["image_url"].as_str())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        model_images.as_slice() == [support::IMAGE_DATA_URL],
        "model-facing MCP output did not preserve the image as typed input"
    );
    anyhow::ensure!(
        cloud.calls.load(Ordering::SeqCst) == 1,
        "expected the configured plugin catalog to be queried once"
    );
    Ok(())
}
