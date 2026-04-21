use crate::extensions::tui_hooks;
use crate::legacy_core::config::Config;
use codex_app_server_client::execute_dynamic_tool_call_for_command;
use codex_app_server_client::failed_dynamic_tool_response;
use codex_app_server_client::load_dynamic_tool_specs_for_command;
use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolSpec;
use tracing::warn;

pub(crate) async fn load_dynamic_tool_specs(config: &Config) -> Vec<DynamicToolSpec> {
    let Some(command) = tui_hooks().dynamic_tool_command(config) else {
        return Vec::new();
    };

    match load_dynamic_tool_specs_for_command(&command.command).await {
        Ok(specs) => specs,
        Err(err) => {
            warn!("failed to load dynamic tool specs: {err}");
            Vec::new()
        }
    }
}

pub(crate) async fn execute_dynamic_tool_call(
    config: &Config,
    params: &DynamicToolCallParams,
) -> DynamicToolCallResponse {
    let Some(command) = tui_hooks().dynamic_tool_command(config) else {
        return failed_dynamic_tool_response("dynamic tool host is unavailable");
    };

    match execute_dynamic_tool_call_for_command(&command.command, params).await {
        Ok(response) => response,
        Err(err) => {
            warn!(
                tool = %params.tool,
                call_id = %params.call_id,
                "dynamic tool call failed: {err}"
            );
            failed_dynamic_tool_response(format!("Dynamic tool `{}` failed: {err}", params.tool))
        }
    }
}
