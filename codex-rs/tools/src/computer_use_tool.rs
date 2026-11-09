use crate::ResponsesApiTool;
use crate::desktop_tool::DESKTOP_OBSERVE_TOOL_NAME;
use crate::desktop_tool::canonical_desktop_dynamic_tool;
use codex_protocol::dynamic_tools::DynamicToolSpec;

pub const COMPUTER_USE_ADAPTER_DESKTOP: &str = "desktop";

const DESKTOP_PROVIDER_TOOLS: &[NativeComputerUseProviderTool] = &[
    NativeComputerUseProviderTool {
        name: DESKTOP_OBSERVE_TOOL_NAME,
        is_mutating: false,
        uses_long_timeout: false,
    },
    NativeComputerUseProviderTool {
        name: crate::desktop_tool::DESKTOP_STEP_TOOL_NAME,
        is_mutating: true,
        uses_long_timeout: false,
    },
];

pub const NATIVE_COMPUTER_USE_PROVIDER_DESKTOP: NativeComputerUseProvider =
    NativeComputerUseProvider {
        adapter: COMPUTER_USE_ADAPTER_DESKTOP,
        tools: DESKTOP_PROVIDER_TOOLS,
        backend_argument: None,
        backend_hints: &[],
    };

const NATIVE_COMPUTER_USE_PROVIDERS: &[NativeComputerUseProvider] = &[
    NATIVE_COMPUTER_USE_PROVIDER_DESKTOP,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeComputerUseProviderTool {
    pub name: &'static str,
    pub is_mutating: bool,
    pub uses_long_timeout: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NativeComputerUseProvider {
    pub adapter: &'static str,
    pub tools: &'static [NativeComputerUseProviderTool],
    pub backend_argument: Option<&'static str>,
    pub backend_hints: &'static [&'static str],
}

impl NativeComputerUseProvider {
    pub fn supports_tool(&self, tool: &str) -> bool {
        self.tool(tool).is_some()
    }

    pub fn supports_call(&self, adapter: &str, tool: &str) -> bool {
        self.adapter == adapter && self.supports_tool(tool)
    }

    pub fn tool(&self, tool: &str) -> Option<&'static NativeComputerUseProviderTool> {
        self.tools.iter().find(|candidate| candidate.name == tool)
    }
}

#[derive(Debug, Clone)]
pub struct NativeComputerUseTool {
    pub adapter: &'static str,
    pub tool: ResponsesApiTool,
    pub is_mutating: bool,
    pub uses_long_timeout: bool,
}

pub fn native_computer_use_provider_registry() -> &'static [NativeComputerUseProvider] {
    NATIVE_COMPUTER_USE_PROVIDERS
}

pub fn native_computer_use_provider_for_adapter(
    adapter: &str,
) -> Option<&'static NativeComputerUseProvider> {
    native_computer_use_provider_registry()
        .iter()
        .find(|provider| provider.adapter == adapter)
}

pub fn native_computer_use_provider_for_tool(
    tool: &str,
) -> Option<(
    &'static NativeComputerUseProvider,
    &'static NativeComputerUseProviderTool,
)> {
    native_computer_use_provider_registry()
        .iter()
        .find_map(|provider| provider.tool(tool).map(|tool| (provider, tool)))
}

pub fn native_computer_use_provider_for_call(
    adapter: &str,
    tool: &str,
) -> Option<(
    &'static NativeComputerUseProvider,
    &'static NativeComputerUseProviderTool,
)> {
    native_computer_use_provider_for_adapter(adapter)
        .and_then(|provider| provider.tool(tool).map(|tool| (provider, tool)))
}

pub fn canonical_native_computer_use_dynamic_tool(
    tool: &DynamicToolSpec,
) -> Option<NativeComputerUseTool> {
    let (provider, provider_tool) = native_computer_use_provider_for_tool(&tool.name)?;
    let output_tool = match provider.adapter {
        COMPUTER_USE_ADAPTER_DESKTOP => canonical_desktop_dynamic_tool(tool)?,
        _ => return None,
    };

    Some(NativeComputerUseTool {
        adapter: provider.adapter,
        tool: output_tool,
        is_mutating: provider_tool.is_mutating,
        uses_long_timeout: provider_tool.uses_long_timeout,
    })
}

#[cfg(test)]
#[path = "computer_use_tool_tests.rs"]
mod tests;
