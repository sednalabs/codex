use super::COMPUTER_USE_ADAPTER_ANDROID;
use super::COMPUTER_USE_ADAPTER_BROWSER;
use super::COMPUTER_USE_ADAPTER_DESKTOP;
use super::canonical_native_computer_use_dynamic_tool;
use super::native_computer_use_provider_for_call;
use super::native_computer_use_provider_registry;
use crate::ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME;
use crate::ANDROID_OBSERVE_TOOL_NAME;
use crate::BROWSER_STEP_TOOL_NAME;
use crate::DESKTOP_OBSERVE_TOOL_NAME;
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

#[test]
fn native_computer_use_provider_registry_declares_adapters_tools_and_backends() {
    let registry = native_computer_use_provider_registry();

    assert_eq!(
        registry
            .iter()
            .map(|provider| provider.adapter)
            .collect::<Vec<_>>(),
        vec![
            COMPUTER_USE_ADAPTER_ANDROID,
            COMPUTER_USE_ADAPTER_BROWSER,
            COMPUTER_USE_ADAPTER_DESKTOP,
        ]
    );

    let (android, android_install) = native_computer_use_provider_for_call(
        COMPUTER_USE_ADAPTER_ANDROID,
        ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME,
    )
    .expect("android install tool should be declared");
    assert_eq!(android.backend_argument, None);
    assert!(android_install.is_mutating);
    assert!(android_install.uses_long_timeout);

    let (browser, browser_step) =
        native_computer_use_provider_for_call(COMPUTER_USE_ADAPTER_BROWSER, BROWSER_STEP_TOOL_NAME)
            .expect("browser step tool should be declared");
    assert_eq!(browser.backend_argument, Some("backend"));
    assert!(browser.backend_hints.contains(&"chrome"));
    assert!(browser_step.is_mutating);

    let (desktop, desktop_observe) = native_computer_use_provider_for_call(
        COMPUTER_USE_ADAPTER_DESKTOP,
        DESKTOP_OBSERVE_TOOL_NAME,
    )
    .expect("desktop observe tool should be declared");
    assert_eq!(desktop.backend_argument, None);
    assert!(!desktop_observe.is_mutating);
}

#[test]
fn native_computer_use_provider_registry_does_not_claim_unknown_tools() {
    assert!(
        native_computer_use_provider_for_call(
            COMPUTER_USE_ADAPTER_BROWSER,
            "browser_private_backend_probe",
        )
        .is_none()
    );
    assert!(
        canonical_native_computer_use_dynamic_tool(&dynamic_tool("browser_private_backend_probe"))
            .is_none()
    );
}
