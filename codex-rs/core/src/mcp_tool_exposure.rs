use std::collections::HashMap;
use std::collections::HashSet;

use codex_connectors::AppToolPolicyEvaluator;
use codex_connectors::AppToolPolicyInput;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::ToolInfo as McpToolInfo;
use codex_mcp::tool_is_model_visible;
use tracing::instrument;
use tracing::warn;

use crate::config::Config;
use crate::connectors;

pub(crate) struct McpToolExposure {
    pub(crate) direct_tools: Vec<McpToolInfo>,
    pub(crate) deferred_tools: Option<Vec<McpToolInfo>>,
}

const PREFERRED_DIRECT_ROUTE_NOTE: &str =
    "Codex uses this direct MCP route because an equivalent app-backed route is also available.";
const DIRECT_ROUTE_DRIFT_NOTE: &str = "This direct MCP route remains visible alongside an app-backed route because their callable contracts differ.";
const APP_ROUTE_DRIFT_NOTE: &str = "This app-backed MCP route remains visible alongside a direct route because their callable contracts differ.";

#[instrument(level = "trace", skip_all)]
pub(crate) fn build_mcp_tool_exposure(
    all_mcp_tools: &[McpToolInfo],
    connectors: Option<&[connectors::AppInfo]>,
    config: &Config,
    search_tool_enabled: bool,
) -> McpToolExposure {
    let direct_tools = filter_non_codex_apps_mcp_tools_only(all_mcp_tools);
    let app_tools = connectors
        .map(|connectors| filter_codex_apps_mcp_tools(all_mcp_tools, connectors, config))
        .unwrap_or_default();
    let deferred_tools = reconcile_direct_and_app_tools(direct_tools, app_tools);

    if !search_tool_enabled {
        return McpToolExposure {
            direct_tools: deferred_tools,
            deferred_tools: None,
        };
    }

    McpToolExposure {
        direct_tools: Vec::new(),
        deferred_tools: (!deferred_tools.is_empty()).then_some(deferred_tools),
    }
}

fn reconcile_direct_and_app_tools(
    mut direct_tools: Vec<McpToolInfo>,
    app_tools: Vec<McpToolInfo>,
) -> Vec<McpToolInfo> {
    let mut direct_indices = HashMap::<String, HashMap<String, usize>>::new();
    for (index, tool) in direct_tools.iter().enumerate() {
        direct_indices
            .entry(tool.server_name.clone())
            .or_default()
            .insert(tool.callable_name.clone(), index);
    }
    let mut retained_app_tools = Vec::new();

    for mut app_tool in app_tools {
        let Some(connector_id) = app_tool.connector_id.as_deref() else {
            retained_app_tools.push(app_tool);
            continue;
        };
        let app_callable_name = app_tool
            .callable_name
            .strip_prefix('_')
            .unwrap_or(app_tool.callable_name.as_str());
        let Some(&direct_index) = direct_indices
            .get(connector_id)
            .and_then(|tools| tools.get(app_callable_name))
        else {
            retained_app_tools.push(app_tool);
            continue;
        };
        let direct_tool = &mut direct_tools[direct_index];

        if same_callable_contract(direct_tool, &app_tool) {
            append_namespace_note(direct_tool, PREFERRED_DIRECT_ROUTE_NOTE);
            continue;
        }

        warn!(
            connector_id,
            direct_tool = %direct_tool.canonical_tool_name(),
            app_tool = %app_tool.canonical_tool_name(),
            "retaining direct and app-backed MCP tools because their callable contracts differ"
        );
        append_namespace_note(direct_tool, DIRECT_ROUTE_DRIFT_NOTE);
        append_namespace_note(&mut app_tool, APP_ROUTE_DRIFT_NOTE);
        retained_app_tools.push(app_tool);
    }

    direct_tools.extend(retained_app_tools);
    direct_tools
}

fn same_callable_contract(direct_tool: &McpToolInfo, app_tool: &McpToolInfo) -> bool {
    direct_tool.tool.title == app_tool.tool.title
        && direct_tool.tool.description == app_tool.tool.description
        && direct_tool.tool.input_schema == app_tool.tool.input_schema
        && direct_tool.tool.output_schema == app_tool.tool.output_schema
        && direct_tool.tool.annotations == app_tool.tool.annotations
        && direct_tool.tool.execution == app_tool.tool.execution
}

fn append_namespace_note(tool: &mut McpToolInfo, note: &str) {
    let current = tool
        .namespace_description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty());
    if current.is_some_and(|description| description.contains(note)) {
        return;
    }
    tool.namespace_description = Some(match current {
        Some(description) if matches!(description.chars().last(), Some('.' | '!' | '?')) => {
            format!("{description} {note}")
        }
        Some(description) => format!("{description}. {note}"),
        None => note.to_string(),
    });
}

fn filter_non_codex_apps_mcp_tools_only(mcp_tools: &[McpToolInfo]) -> Vec<McpToolInfo> {
    mcp_tools
        .iter()
        .filter(|tool| {
            tool.server_name != CODEX_APPS_MCP_SERVER_NAME && tool_is_model_visible(tool)
        })
        .cloned()
        .collect()
}

fn filter_codex_apps_mcp_tools(
    mcp_tools: &[McpToolInfo],
    connectors: &[connectors::AppInfo],
    config: &Config,
) -> Vec<McpToolInfo> {
    let allowed: HashSet<&str> = connectors
        .iter()
        .map(|connector| connector.id.as_str())
        .collect();
    let app_tool_policy = AppToolPolicyEvaluator::new(&config.config_layer_stack);

    mcp_tools
        .iter()
        .filter(|tool| {
            if tool.server_name != CODEX_APPS_MCP_SERVER_NAME {
                return false;
            }
            if !tool_is_model_visible(tool) {
                return false;
            }
            let Some(connector_id) = tool.connector_id.as_deref() else {
                return false;
            };
            let annotations = tool.tool.annotations.as_ref();
            allowed.contains(connector_id)
                && app_tool_policy
                    .policy(AppToolPolicyInput {
                        connector_id: Some(connector_id),
                        tool_name: &tool.tool.name,
                        tool_title: tool.tool.title.as_deref(),
                        destructive_hint: annotations
                            .and_then(|annotations| annotations.destructive_hint),
                        open_world_hint: annotations
                            .and_then(|annotations| annotations.open_world_hint),
                    })
                    .enabled
        })
        .cloned()
        .collect()
}

#[cfg(test)]
#[path = "mcp_tool_exposure_test.rs"]
mod tests;
