use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;

use codex_connectors::AppToolPolicyEvaluator;
use codex_connectors::AppToolPolicyInput;
use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpBinding;
use codex_mcp::ToolInfo as McpToolInfo;
use codex_mcp::tool_is_model_visible;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use tracing::instrument;
use tracing::warn;

const MAX_AGENT_PLUGIN_MCP_SPEC_BYTES: usize = 8_000;
const MAX_AGENT_PLUGIN_MCP_TOTAL_BYTES: usize = 64_000;

use crate::config::Config;
use crate::tools::handlers::McpHandler;
use crate::tools::registry::ToolRegistry;

#[derive(Default)]
pub(crate) struct McpHandlerCache {
    cached: Mutex<Option<CachedMcpHandlers>>,
}

struct CachedMcpHandlers {
    binding: Weak<McpBinding>,
    handlers: HashMap<ToolName, Arc<McpHandler>>,
}

impl McpHandlerCache {
    pub(crate) fn append_mcp_tools(
        &self,
        binding: &Arc<McpBinding>,
        config: &Config,
        apps_enabled: bool,
        mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
        search_tool_enabled: bool,
        registry: &mut ToolRegistry,
    ) -> HashSet<ToolName> {
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !cached
            .as_ref()
            .and_then(|cached| cached.binding.upgrade())
            .is_some_and(|cached_binding| Arc::ptr_eq(&cached_binding, binding))
        {
            *cached = None;
        }

        let cached = cached.get_or_insert_with(|| CachedMcpHandlers {
            binding: Arc::downgrade(binding),
            handlers: HashMap::new(),
        });
        append_mcp_tools(
            binding.tools(),
            config,
            apps_enabled,
            mcp_server_catalog,
            search_tool_enabled,
            &mut cached.handlers,
            registry,
        )
    }
}

const PREFERRED_DIRECT_ROUTE_NOTE: &str =
    "Codex uses this direct MCP route because an equivalent app-backed route is also available.";
const DIRECT_ROUTE_DRIFT_NOTE: &str = "This direct MCP route remains visible alongside an app-backed route because their callable contracts differ.";
const APP_ROUTE_DRIFT_NOTE: &str = "This app-backed MCP route remains visible alongside a direct route because their callable contracts differ.";

#[instrument(level = "trace", skip_all)]
fn append_mcp_tools(
    all_mcp_tools: &[McpToolInfo],
    config: &Config,
    apps_enabled: bool,
    mcp_server_catalog: &codex_mcp::ResolvedMcpCatalog,
    search_tool_enabled: bool,
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
) -> Vec<Arc<dyn CoreToolRuntime>> {
    let direct_tools = filter_non_codex_apps_mcp_tools_only(all_mcp_tools);
    let app_tools = connectors
        .map(|connectors| filter_codex_apps_mcp_tools(all_mcp_tools, connectors, config))
        .unwrap_or_default();
    let exposed_tools = reconcile_direct_and_app_tools(direct_tools, app_tools);

=======
    handlers: &mut HashMap<ToolName, Arc<McpHandler>>,
    registry: &mut ToolRegistry,
) -> HashSet<ToolName> {
    // Keep regular MCP tools first; Apps tools also require connector and policy checks.
    let non_app_tools = filter_non_codex_apps_mcp_tools_only(all_mcp_tools);
    let app_tools = apps_enabled
        .then(|| filter_codex_apps_mcp_tools(all_mcp_tools, config))
        .into_iter()
        .flatten();
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    let exposure = if search_tool_enabled {
        ToolExposure::Deferred
    } else {
        ToolExposure::Direct
    };
    let mut registered_tools = HashSet::new();
    let mut agent_plugin_bytes = 0usize;
    for tool in non_app_tools.chain(app_tools) {
        let tool_name = tool.canonical_tool_name();
        let agent_plugin = mcp_server_catalog
            .server(&tool.server_name)
            .is_some_and(|server| server.source().is_agent_plugin());
        let handler = match handlers.entry(tool_name.clone()) {
            Entry::Occupied(entry) => Arc::clone(entry.get()),
            Entry::Vacant(entry) => {
                let handler = if agent_plugin {
                    McpHandler::new_agent_plugin(tool.clone())
                } else {
                    McpHandler::new(tool.clone())
                };

                match handler {
                    Ok(handler) => Arc::clone(entry.insert(Arc::new(handler))),
                    Err(err) => {
                        warn!("Skipping MCP tool `{tool_name}`: failed to build tool spec: {err}");
                        continue;
                    }
                }
            }
        };

        let fits_agent_budget = if agent_plugin {
            handler.model_spec_bytes().is_ok_and(|bytes| {
                if bytes > MAX_AGENT_PLUGIN_MCP_SPEC_BYTES {
                    return false;
                }
                let next = agent_plugin_bytes.saturating_add(bytes);
                if next <= MAX_AGENT_PLUGIN_MCP_TOTAL_BYTES {
                    agent_plugin_bytes = next;
                    true
                } else {
                    false
                }
            })
        } else {
            true
        };
        let tool_exposure = if fits_agent_budget {
            exposure
        } else {
            ToolExposure::Hidden
        };
        if registry.register_external_with_exposure(handler, tool_exposure) && fits_agent_budget {
            registered_tools.insert(tool_name);
        }
    }
    registered_tools
}

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
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
=======
fn filter_non_codex_apps_mcp_tools_only(
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
    mcp_tools: &[McpToolInfo],
) -> impl Iterator<Item = &McpToolInfo> + '_ {
    mcp_tools.iter().filter(|tool| {
        tool.server_name != CODEX_APPS_MCP_SERVER_NAME && tool_is_model_visible(tool)
    })
}

fn filter_codex_apps_mcp_tools<'a>(
    mcp_tools: &'a [McpToolInfo],
    config: &'a Config,
) -> impl Iterator<Item = &'a McpToolInfo> + 'a {
    let app_tool_policy = AppToolPolicyEvaluator::new(&config.config_layer_stack);

    mcp_tools.iter().filter(move |tool| {
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
        app_tool_policy
            .policy(AppToolPolicyInput {
                connector_id: Some(connector_id),
                link_id: None,
                tool_name: &tool.tool.name,
                tool_title: tool.tool.title.as_deref(),
                destructive_hint: annotations.and_then(|annotations| annotations.destructive_hint),
                open_world_hint: annotations.and_then(|annotations| annotations.open_world_hint),
            })
            .enabled
    })
}

#[cfg(test)]
#[path = "mcp_tool_exposure_test.rs"]
mod tests;
