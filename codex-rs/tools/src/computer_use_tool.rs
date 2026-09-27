use crate::ResponsesApiTool;
use crate::android_tool::ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME;
use crate::android_tool::ANDROID_OBSERVE_TOOL_NAME;
use crate::android_tool::canonical_android_dynamic_tool;
use crate::browser_tool::BROWSER_OBSERVE_TOOL_NAME;
use crate::browser_tool::canonical_browser_dynamic_tool;
use crate::desktop_tool::DESKTOP_OBSERVE_TOOL_NAME;
use crate::desktop_tool::canonical_desktop_dynamic_tool;
use codex_protocol::dynamic_tools::DynamicToolSpec;

pub const COMPUTER_USE_ADAPTER_ANDROID: &str = "android";
pub const COMPUTER_USE_ADAPTER_BROWSER: &str = "browser";
pub const COMPUTER_USE_ADAPTER_DESKTOP: &str = "desktop";

const COMPUTER_USE_BACKEND_AUTO: &str = "auto";
const COMPUTER_USE_BACKEND_BROWSER: &str = "browser";
const COMPUTER_USE_BACKEND_CHROME: &str = "chrome";
const COMPUTER_USE_BACKEND_CHROMIUM: &str = "chromium";
const COMPUTER_USE_BACKEND_IAB: &str = "iab";

const ANDROID_PROVIDER_TOOLS: &[NativeComputerUseProviderTool] = &[
    NativeComputerUseProviderTool {
        name: ANDROID_OBSERVE_TOOL_NAME,
        is_mutating: false,
        uses_long_timeout: false,
    },
    NativeComputerUseProviderTool {
        name: crate::android_tool::ANDROID_STEP_TOOL_NAME,
        is_mutating: true,
        uses_long_timeout: false,
    },
    NativeComputerUseProviderTool {
        name: ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME,
        is_mutating: true,
        uses_long_timeout: true,
    },
];

const BROWSER_PROVIDER_BACKENDS: &[&str] = &[
    COMPUTER_USE_BACKEND_AUTO,
    COMPUTER_USE_BACKEND_BROWSER,
    COMPUTER_USE_BACKEND_CHROME,
    COMPUTER_USE_BACKEND_CHROMIUM,
    COMPUTER_USE_BACKEND_IAB,
];

const BROWSER_PROVIDER_TOOLS: &[NativeComputerUseProviderTool] = &[
    NativeComputerUseProviderTool {
        name: BROWSER_OBSERVE_TOOL_NAME,
        is_mutating: false,
        uses_long_timeout: false,
    },
    NativeComputerUseProviderTool {
        name: crate::browser_tool::BROWSER_STEP_TOOL_NAME,
        is_mutating: true,
        uses_long_timeout: false,
    },
];

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

pub const NATIVE_COMPUTER_USE_PROVIDER_ANDROID: NativeComputerUseProvider =
    NativeComputerUseProvider {
        adapter: COMPUTER_USE_ADAPTER_ANDROID,
        tools: ANDROID_PROVIDER_TOOLS,
        backend_argument: None,
        backend_hints: &[],
    };

pub const NATIVE_COMPUTER_USE_PROVIDER_BROWSER: NativeComputerUseProvider =
    NativeComputerUseProvider {
        adapter: COMPUTER_USE_ADAPTER_BROWSER,
        tools: BROWSER_PROVIDER_TOOLS,
        backend_argument: Some("backend"),
        backend_hints: BROWSER_PROVIDER_BACKENDS,
    };

pub const NATIVE_COMPUTER_USE_PROVIDER_DESKTOP: NativeComputerUseProvider =
    NativeComputerUseProvider {
        adapter: COMPUTER_USE_ADAPTER_DESKTOP,
        tools: DESKTOP_PROVIDER_TOOLS,
        backend_argument: None,
        backend_hints: &[],
    };

const NATIVE_COMPUTER_USE_PROVIDERS: &[NativeComputerUseProvider] = &[
    NATIVE_COMPUTER_USE_PROVIDER_ANDROID,
    NATIVE_COMPUTER_USE_PROVIDER_BROWSER,
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
        COMPUTER_USE_ADAPTER_ANDROID => canonical_android_dynamic_tool(tool)?,
        COMPUTER_USE_ADAPTER_BROWSER => canonical_browser_dynamic_tool(tool)?,
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
