use crate::ResponsesApiTool;
use crate::android_tool::ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME;
use crate::android_tool::ANDROID_OBSERVE_TOOL_NAME;
use crate::android_tool::canonical_android_dynamic_tool;
use crate::browser_tool::BROWSER_OBSERVE_TOOL_NAME;
use crate::browser_tool::canonical_browser_dynamic_tool;
use codex_protocol::dynamic_tools::DynamicToolSpec;

pub const COMPUTER_USE_ADAPTER_ANDROID: &str = "android";
pub const COMPUTER_USE_ADAPTER_BROWSER: &str = "browser";

#[derive(Debug, Clone)]
pub struct NativeComputerUseTool {
    pub adapter: &'static str,
    pub tool: ResponsesApiTool,
    pub is_mutating: bool,
    pub uses_long_timeout: bool,
}

pub fn canonical_native_computer_use_dynamic_tool(
    tool: &DynamicToolSpec,
) -> Option<NativeComputerUseTool> {
    if let Some(output_tool) = canonical_android_dynamic_tool(tool) {
        let is_observe = output_tool.name == ANDROID_OBSERVE_TOOL_NAME;
        let uses_long_timeout = output_tool.name == ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME;
        return Some(NativeComputerUseTool {
            adapter: COMPUTER_USE_ADAPTER_ANDROID,
            tool: output_tool,
            is_mutating: !is_observe,
            uses_long_timeout,
        });
    }

    canonical_browser_dynamic_tool(tool).map(|output_tool| {
        let is_observe = output_tool.name == BROWSER_OBSERVE_TOOL_NAME;
        NativeComputerUseTool {
            adapter: COMPUTER_USE_ADAPTER_BROWSER,
            tool: output_tool,
            is_mutating: !is_observe,
            uses_long_timeout: false,
        }
    })
}

#[cfg(test)]
#[path = "computer_use_tool_tests.rs"]
mod tests;
