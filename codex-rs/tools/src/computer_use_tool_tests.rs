use super::COMPUTER_USE_ADAPTER_ANDROID;
use super::COMPUTER_USE_ADAPTER_BROWSER;
use super::COMPUTER_USE_ADAPTER_DESKTOP;
use super::canonical_native_computer_use_dynamic_tool;
use crate::ANDROID_OBSERVE_TOOL_NAME;
use crate::BROWSER_STEP_TOOL_NAME;
use crate::DESKTOP_STEP_TOOL_NAME;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use pretty_assertions::assert_eq;
use serde_json::json;

fn dynamic_tool(name: &str) -> DynamicToolSpec {
    DynamicToolSpec {
        namespace: None,
        name: name.to_string(),
        description: format!("{name} dynamic tool"),
        input_schema: json!({ "type": "object" }),
        defer_loading: false,
        persist_on_resume: true,
        capability: None,
    }
}

#[test]
fn native_computer_use_registry_classifies_android_and_browser_tools() {
    let android =
        canonical_native_computer_use_dynamic_tool(&dynamic_tool(ANDROID_OBSERVE_TOOL_NAME))
            .expect("android observe should be native computer-use");
    let browser = canonical_native_computer_use_dynamic_tool(&dynamic_tool(BROWSER_STEP_TOOL_NAME))
        .expect("browser step should be native computer-use");

    assert_eq!(android.adapter, COMPUTER_USE_ADAPTER_ANDROID);
    assert!(!android.is_mutating);
    assert_eq!(browser.adapter, COMPUTER_USE_ADAPTER_BROWSER);
    assert!(browser.is_mutating);
}

#[test]
fn native_computer_use_registry_classifies_desktop_tools() {
    let desktop = canonical_native_computer_use_dynamic_tool(&dynamic_tool(DESKTOP_STEP_TOOL_NAME))
        .expect("desktop step should be native computer-use");

    assert_eq!(desktop.adapter, COMPUTER_USE_ADAPTER_DESKTOP);
    assert!(desktop.is_mutating);
    assert!(!desktop.uses_long_timeout);
}
