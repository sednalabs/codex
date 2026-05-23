use super::BROWSER_OBSERVE_TOOL_NAME;
use super::BROWSER_STEP_TOOL_NAME;
use super::canonical_browser_dynamic_tool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn canonical_browser_dynamic_tool_preserves_supported_browser_tool_names() {
    let observe = canonical_browser_dynamic_tool(&DynamicToolSpec {
        namespace: None,
        name: BROWSER_OBSERVE_TOOL_NAME.to_string(),
        description: "custom observe description".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: false,
        persist_on_resume: false,
        capability: None,
    })
    .expect("canonical observe tool");

    assert_eq!(observe.name, BROWSER_OBSERVE_TOOL_NAME);
    assert!(observe.description.contains("model-visible screenshot"));
    let observe_properties = observe
        .parameters
        .properties
        .expect("observe properties should be present");
    assert!(observe_properties.contains_key("backend"));
    assert!(observe_properties.contains_key("scope"));
    assert!(observe_properties.contains_key("prompt"));

    let step = canonical_browser_dynamic_tool(&DynamicToolSpec {
        namespace: None,
        name: BROWSER_STEP_TOOL_NAME.to_string(),
        description: "custom step description".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: true,
        persist_on_resume: false,
        capability: None,
    })
    .expect("canonical step tool");

    assert_eq!(step.name, BROWSER_STEP_TOOL_NAME);
    assert!(step.description.contains("bounded browser actions"));
    assert_eq!(step.defer_loading, Some(true));
    let step_properties = step
        .parameters
        .properties
        .expect("step properties should be present");
    assert!(step_properties.contains_key("actions"));
    assert!(step_properties.contains_key("view"));
    assert!(step_properties.contains_key("action"));
    assert!(step_properties.contains_key("backend"));

    let action_schema = step_properties.get("action").expect("action schema");
    let action_values = action_schema
        .enum_values
        .as_ref()
        .expect("action enum values");
    for value in ["navigate", "click", "type", "keypress", "scroll", "wait"] {
        assert!(
            action_values.contains(&json!(value)),
            "missing browser_step action value {value}"
        );
    }
}

#[test]
fn browser_backend_schema_exposes_supported_provider_backends() {
    let observe = canonical_browser_dynamic_tool(&DynamicToolSpec {
        namespace: None,
        name: BROWSER_OBSERVE_TOOL_NAME.to_string(),
        description: "custom observe description".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: false,
        persist_on_resume: false,
        capability: None,
    })
    .expect("canonical observe tool");
    let step = canonical_browser_dynamic_tool(&DynamicToolSpec {
        namespace: None,
        name: BROWSER_STEP_TOOL_NAME.to_string(),
        description: "custom step description".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: false,
        persist_on_resume: false,
        capability: None,
    })
    .expect("canonical step tool");

    let observe_backend_schema = observe
        .parameters
        .properties
        .as_ref()
        .expect("observe properties")
        .get("backend")
        .expect("observe backend schema");
    let step_backend_schema = step
        .parameters
        .properties
        .as_ref()
        .expect("step properties")
        .get("backend")
        .expect("step backend schema");
    assert_eq!(observe_backend_schema, step_backend_schema);
    assert_eq!(
        observe_backend_schema.enum_values,
        Some(vec![json!("auto"), json!("iab"), json!("chrome")])
    );
}

#[test]
fn canonical_browser_dynamic_tool_ignores_namespaced_browser_names() {
    let tool = canonical_browser_dynamic_tool(&DynamicToolSpec {
        namespace: Some("codex_app".to_string()),
        name: BROWSER_OBSERVE_TOOL_NAME.to_string(),
        description: "custom namespaced observe".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: false,
        persist_on_resume: true,
        capability: None,
    });

    assert!(tool.is_none());
}
