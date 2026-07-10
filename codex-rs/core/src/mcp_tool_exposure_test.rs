use std::collections::HashSet;
use std::sync::Arc;

use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo;
use codex_tools::ToolName;
use pretty_assertions::assert_eq;
use rmcp::model::Icon;
use rmcp::model::JsonObject;
use rmcp::model::Meta;
use rmcp::model::Tool;

use super::*;
use crate::config::CONFIG_TOML_FILE;
use crate::config::ConfigBuilder;
use crate::config::test_config;
use crate::connectors::AppInfo;
use tempfile::tempdir;

fn make_connector(id: &str, name: &str) -> AppInfo {
    AppInfo {
        id: id.to_string(),
        name: name.to_string(),
        description: None,
        logo_url: None,
        logo_url_dark: None,
        icon_assets: None,
        icon_dark_assets: None,
        distribution_channel: None,
        branding: None,
        app_metadata: None,
        labels: None,
        install_url: None,
        is_accessible: true,
        is_enabled: true,
        plugin_display_names: Vec::new(),
    }
}

fn make_mcp_tool(
    server_name: &str,
    tool_name: &str,
    callable_namespace: &str,
    callable_name: &str,
    connector_id: Option<&str>,
    connector_name: Option<&str>,
) -> ToolInfo {
    ToolInfo {
        server_name: server_name.to_string(),
        supports_parallel_tool_calls: false,
        server_origin: None,
        callable_name: callable_name.to_string(),
        callable_namespace: callable_namespace.to_string(),
        namespace_description: None,
        tool: Tool::new(
            tool_name.to_string(),
            format!("Test tool: {tool_name}"),
            Arc::new(JsonObject::default()),
        ),
        connector_id: connector_id.map(str::to_string),
        connector_name: connector_name.map(str::to_string),
        plugin_display_names: Vec::new(),
    }
}

fn numbered_mcp_tools(count: usize) -> Vec<ToolInfo> {
    (0..count)
        .map(|index| {
            let tool_name = format!("tool_{index}");
            make_mcp_tool(
                "rmcp",
                &tool_name,
                "mcp__rmcp",
                &tool_name,
                /*connector_id*/ None,
                /*connector_name*/ None,
            )
        })
        .collect()
}

fn tool_names(tools: &[ToolInfo]) -> HashSet<ToolName> {
    tools
        .iter()
        .map(codex_mcp::ToolInfo::canonical_tool_name)
        .collect()
}

fn with_visibility(mut tool: ToolInfo, visibility: &[&str]) -> ToolInfo {
    tool.tool.meta = Some(Meta(
        serde_json::json!({ "ui": { "visibility": visibility } })
            .as_object()
            .expect("metadata object")
            .clone(),
    ));
    tool
}

fn equivalent_ops_tool_pair() -> (ToolInfo, ToolInfo) {
    let mut direct_tool = make_mcp_tool(
        "ops",
        "work_items_read",
        "mcp__ops",
        "work_items_read",
        /*connector_id*/ None,
        /*connector_name*/ None,
    );
    direct_tool.supports_parallel_tool_calls = true;
    direct_tool.namespace_description = Some("Direct Ops tools".to_string());

    let mut app_tool = make_mcp_tool(
        CODEX_APPS_MCP_SERVER_NAME,
        "ops_work_items_read",
        "mcp__codex_apps__ops",
        "_work_items_read",
        Some("ops"),
        Some("Ops"),
    );
    app_tool.namespace_description = Some("App-backed Ops tools".to_string());
    app_tool.tool.description = direct_tool.tool.description.clone();
    app_tool.tool.icons = Some(vec![Icon::new("https://example.test/ops.png")]);

    (direct_tool, app_tool)
}

fn assert_tool_infos_eq(actual: &[ToolInfo], expected: &[ToolInfo]) {
    assert_eq!(
        serde_json::to_value(actual).expect("serialize actual tool inventory"),
        serde_json::to_value(expected).expect("serialize expected tool inventory")
    );
}

#[tokio::test]
async fn preserves_direct_only_tool_inventory() {
    let config = test_config().await;
    let (direct_tool, _) = equivalent_ops_tool_pair();

    let exposure = build_mcp_tool_exposure(
        std::slice::from_ref(&direct_tool),
        /*connectors*/ None,
        &config,
        /*search_tool_enabled*/ false,
    );

    assert_tool_infos_eq(&exposure.direct_tools, &[direct_tool]);
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn preserves_app_only_tool_inventory_as_fallback() {
    let config = test_config().await;
    let (_, app_tool) = equivalent_ops_tool_pair();
    let connectors = vec![make_connector("ops", "Ops")];

    let exposure = build_mcp_tool_exposure(
        std::slice::from_ref(&app_tool),
        Some(connectors.as_slice()),
        &config,
        /*search_tool_enabled*/ false,
    );

    assert_tool_infos_eq(&exposure.direct_tools, &[app_tool]);
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn prefers_one_provenance_visible_direct_tool_for_equivalent_routes() {
    let config = test_config().await;
    let connectors = vec![make_connector("ops", "Ops")];
    let (direct_tool, app_tool) = equivalent_ops_tool_pair();
    let mut expected_tool = direct_tool.clone();
    append_namespace_note(&mut expected_tool, PREFERRED_DIRECT_ROUTE_NOTE);

    for search_tool_enabled in [false, true] {
        let exposure = build_mcp_tool_exposure(
            &[direct_tool.clone(), app_tool.clone()],
            Some(connectors.as_slice()),
            &config,
            search_tool_enabled,
        );

        if search_tool_enabled {
            assert!(exposure.direct_tools.is_empty());
            assert_tool_infos_eq(
                exposure
                    .deferred_tools
                    .as_deref()
                    .expect("preferred route should remain searchable"),
                std::slice::from_ref(&expected_tool),
            );
        } else {
            assert_tool_infos_eq(
                &exposure.direct_tools,
                std::slice::from_ref(&expected_tool),
            );
            assert!(exposure.deferred_tools.is_none());
        }
    }
}

#[tokio::test]
async fn retains_and_labels_both_routes_when_callable_schemas_drift() {
    let config = test_config().await;
    let connectors = vec![make_connector("ops", "Ops")];
    let (mut direct_tool, mut app_tool) = equivalent_ops_tool_pair();
    app_tool.tool.input_schema = Arc::new(
        serde_json::json!({
            "type": "object",
            "properties": {
                "project_id": { "type": "integer" }
            }
        })
        .as_object()
        .expect("input schema object")
        .clone(),
    );
    let source_tools = vec![direct_tool.clone(), app_tool.clone()];
    append_namespace_note(&mut direct_tool, DIRECT_ROUTE_DRIFT_NOTE);
    append_namespace_note(&mut app_tool, APP_ROUTE_DRIFT_NOTE);

    let exposure = build_mcp_tool_exposure(
        &source_tools,
        Some(connectors.as_slice()),
        &config,
        /*search_tool_enabled*/ true,
    );

    assert!(exposure.direct_tools.is_empty());
    assert_tool_infos_eq(
        exposure
            .deferred_tools
            .as_deref()
            .expect("drifted routes should both remain searchable"),
        &[direct_tool, app_tool],
    );
}

#[tokio::test]
async fn directly_exposes_effective_tool_sets_when_search_is_unavailable() {
    let config = test_config().await;
    let mcp_tools = numbered_mcp_tools(/*count*/ 2);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools, /*connectors*/ None, &config, /*search_tool_enabled*/ false,
    );

    assert_eq!(tool_names(&exposure.direct_tools), tool_names(&mcp_tools));
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn excludes_tools_hidden_from_model_exposure() {
    let config = test_config().await;
    let visible_tool = make_mcp_tool(
        "rmcp",
        "visible_tool",
        "mcp__rmcp",
        "visible_tool",
        /*connector_id*/ None,
        /*connector_name*/ None,
    );
    let hidden_tool = with_visibility(
        make_mcp_tool(
            "rmcp",
            "hidden_tool",
            "mcp__rmcp",
            "hidden_tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        &["app"],
    );
    let empty_visibility_tool = with_visibility(
        make_mcp_tool(
            "rmcp",
            "empty_visibility_tool",
            "mcp__rmcp",
            "empty_visibility_tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        &[],
    );
    let visible_app_tool = with_visibility(
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_read",
            "mcp__codex_apps__calendar",
            "read",
            Some("calendar"),
            Some("Calendar"),
        ),
        &["app", "model"],
    );
    let hidden_app_tool = with_visibility(
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_open",
            "mcp__codex_apps__calendar",
            "open",
            Some("calendar"),
            Some("Calendar"),
        ),
        &["app"],
    );
    let mcp_tools = vec![
        visible_tool.clone(),
        hidden_tool,
        empty_visibility_tool,
        visible_app_tool.clone(),
        hidden_app_tool,
    ];
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        Some(connectors.as_slice()),
        &config,
        /*search_tool_enabled*/ false,
    );

    assert_eq!(
        tool_names(&exposure.direct_tools),
        tool_names(&[visible_tool, visible_app_tool])
    );
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn applies_per_tool_app_policy_across_the_exposure_build() {
    let codex_home = tempdir().expect("tempdir should succeed");
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        r#"
[apps.calendar]
default_tools_enabled = false

[apps.calendar.tools."events/create"]
enabled = true
"#,
    )
    .expect("write config");
    let config = ConfigBuilder::default()
        .codex_home(codex_home.path().to_path_buf())
        .build()
        .await
        .expect("config should build");
    let enabled_tool = make_mcp_tool(
        CODEX_APPS_MCP_SERVER_NAME,
        "events/create",
        "mcp__codex_apps__calendar",
        "create",
        Some("calendar"),
        Some("Calendar"),
    );
    let disabled_tool = make_mcp_tool(
        CODEX_APPS_MCP_SERVER_NAME,
        "events/list",
        "mcp__codex_apps__calendar",
        "list",
        Some("calendar"),
        Some("Calendar"),
    );
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &[enabled_tool.clone(), disabled_tool],
        Some(connectors.as_slice()),
        &config,
        /*search_tool_enabled*/ false,
    );

    assert_eq!(
        tool_names(&exposure.direct_tools),
        tool_names(&[enabled_tool])
    );
    assert!(exposure.deferred_tools.is_none());
}

#[tokio::test]
async fn defers_effective_tool_sets_when_search_is_available() {
    let config = test_config().await;
    let mcp_tools = numbered_mcp_tools(/*count*/ 2);

    let exposure = build_mcp_tool_exposure(
        &mcp_tools, /*connectors*/ None, &config, /*search_tool_enabled*/ true,
    );

    assert!(exposure.direct_tools.is_empty());
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("MCP tools should be discoverable through tool_search");
    assert_eq!(tool_names(deferred_tools), tool_names(&mcp_tools));
}

#[tokio::test]
async fn defers_apps_and_non_app_mcp_tools() {
    let config = test_config().await;
    let mcp_tools = vec![
        make_mcp_tool(
            "rmcp",
            "tool",
            "mcp__rmcp",
            "tool",
            /*connector_id*/ None,
            /*connector_name*/ None,
        ),
        make_mcp_tool(
            CODEX_APPS_MCP_SERVER_NAME,
            "calendar_create_event",
            "mcp__codex_apps__calendar",
            "_create_event",
            Some("calendar"),
            Some("Calendar"),
        ),
    ];
    let connectors = vec![make_connector("calendar", "Calendar")];

    let exposure = build_mcp_tool_exposure(
        &mcp_tools,
        Some(connectors.as_slice()),
        &config,
        /*search_tool_enabled*/ true,
    );

    assert!(exposure.direct_tools.is_empty());
    let deferred_tools = exposure
        .deferred_tools
        .as_ref()
        .expect("MCP tools should be discoverable through tool_search");
    let deferred_tool_names = tool_names(deferred_tools);
    assert!(deferred_tool_names.contains(&ToolName::namespaced("mcp__rmcp", "tool")));
    assert!(deferred_tool_names.contains(&ToolName::namespaced(
        "mcp__codex_apps__calendar",
        "_create_event"
    )));
}
