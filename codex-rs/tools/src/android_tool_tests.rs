use super::ANDROID_OBSERVE_TOOL_NAME;
use super::ANDROID_STEP_TOOL_NAME;
use super::canonical_android_dynamic_tool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use serde_json::json;

#[test]
fn canonical_android_dynamic_tool_preserves_supported_android_tool_names() {
    let observe = canonical_android_dynamic_tool(&DynamicToolSpec {
        name: ANDROID_OBSERVE_TOOL_NAME.to_string(),
        description: "custom observe description".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: false,
        persist_on_resume: false,
        capability: None,
    })
    .expect("canonical observe tool");

    assert_eq!(observe.name, ANDROID_OBSERVE_TOOL_NAME);
    assert!(observe.description.contains("model-visible screenshot"));
    let observe_properties = observe
        .parameters
        .properties
        .expect("observe properties should be present");
    assert!(observe_properties.contains_key("scope"));
    assert!(observe_properties.contains_key("prompt"));

    let step = canonical_android_dynamic_tool(&DynamicToolSpec {
        name: ANDROID_STEP_TOOL_NAME.to_string(),
        description: "custom step description".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: true,
        persist_on_resume: false,
        capability: None,
    })
    .expect("canonical step tool");

    assert_eq!(step.name, ANDROID_STEP_TOOL_NAME);
    assert!(step.description.contains("bounded Android actions"));
    assert_eq!(step.defer_loading, Some(true));
    let step_properties = step
        .parameters
        .properties
        .expect("step properties should be present");
    assert!(step_properties.contains_key("actions"));
    assert!(step_properties.contains_key("view"));
    assert!(step_properties.contains_key("action"));
}

#[test]
fn canonical_android_dynamic_tool_ignores_non_android_tools() {
    let tool = canonical_android_dynamic_tool(&DynamicToolSpec {
        name: "other_tool".to_string(),
        description: "other".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: false,
        persist_on_resume: true,
        capability: None,
    });

    assert!(tool.is_none());
}
