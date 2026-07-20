use super::ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME;
use super::ANDROID_OBSERVE_TOOL_NAME;
use super::ANDROID_STEP_TOOL_NAME;
use super::canonical_android_dynamic_tool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn canonical_android_dynamic_tool_preserves_supported_android_tool_names() {
    let observe = canonical_android_dynamic_tool(&DynamicToolSpec {
        namespace: None,
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
    for property in [
        "scope",
        "prompt",
        "serial",
        "timeout_secs",
        "screenshot_filename",
        "hierarchy_filename",
    ] {
        assert!(
            observe_properties.contains_key(property),
            "missing observe property {property}"
        );
    }

    let step = canonical_android_dynamic_tool(&DynamicToolSpec {
        namespace: None,
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
    for property in [
        "actions",
        "view",
        "action",
        "screenshot_filename",
        "hierarchy_filename",
    ] {
        assert!(
            step_properties.contains_key(property),
            "missing step property {property}"
        );
    }

    let action_schema = step_properties.get("action").expect("action schema");
    let action_values = action_schema
        .enum_values
        .as_ref()
        .expect("action enum values");
    for value in [
        "tap",
        "type_text",
        "key",
        "swipe",
        "multi_touch",
        "click",
        "long_press",
        "zoom",
        "reset_zoom",
    ] {
        assert!(
            action_values.contains(&json!(value)),
            "missing android_step action value {value}"
        );
    }

    let actions_schema = step_properties.get("actions").expect("actions schema");
    let action_item_properties = actions_schema
        .items
        .as_ref()
        .and_then(|item| item.properties.as_ref())
        .expect("actions[] item properties");
    for property in [
        "type", "region", "frame", "key", "ms", "package", "pointers", "selector",
    ] {
        assert!(
            action_item_properties.contains_key(property),
            "missing actions[] property {property}"
        );
    }
    let pointer_schema = action_item_properties
        .get("pointers")
        .and_then(|schema| schema.items.as_ref())
        .expect("multi-touch pointer schema");
    let pointer_properties = pointer_schema
        .properties
        .as_ref()
        .expect("multi-touch pointer properties");
    assert_eq!(
        pointer_properties.keys().cloned().collect::<Vec<_>>(),
        vec![
            "x1".to_string(),
            "x2".to_string(),
            "y1".to_string(),
            "y2".to_string(),
        ]
    );
    assert_eq!(
        pointer_schema.required.as_deref(),
        Some(
            &[
                "x1".to_string(),
                "y1".to_string(),
                "x2".to_string(),
                "y2".to_string(),
            ][..]
        )
    );
    let selector_properties = action_item_properties
        .get("selector")
        .and_then(|schema| schema.properties.as_ref())
        .expect("selector properties");
    for property in [
        "text",
        "content_description",
        "contentDescription",
        "resource_id",
        "resourceId",
        "class_name",
        "className",
        "bounds",
    ] {
        assert!(
            selector_properties.contains_key(property),
            "missing selector property {property}"
        );
    }

    let view_properties = step_properties
        .get("view")
        .and_then(|schema| schema.properties.as_ref())
        .expect("view properties");
    for property in [
        "deviceWidth",
        "device_height",
        "device",
        "frameWidth",
        "frame_height",
        "frame",
        "region",
        "zoomed",
    ] {
        assert!(
            view_properties.contains_key(property),
            "missing view property {property}"
        );
    }

    let install = canonical_android_dynamic_tool(&DynamicToolSpec {
        namespace: None,
        name: ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME.to_string(),
        description: "custom install description".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: true,
        persist_on_resume: false,
        capability: None,
    })
    .expect("canonical install tool");

    assert_eq!(install.name, ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME);
    assert!(
        install
            .description
            .contains("GitHub Actions Android build artifact")
    );
    assert_eq!(install.defer_loading, Some(true));
    assert_eq!(
        install.parameters.required.as_deref(),
        Some(&["workflow_run_id".to_string(), "artifact_name".to_string()][..])
    );
    let install_properties = install
        .parameters
        .properties
        .expect("install properties should be present");
    for property in [
        "workflow_run_id",
        "artifact_name",
        "repository",
        "serial",
        "launch_after_install",
        "timeout_secs",
        "post_observe_scope",
        "screenshot_filename",
        "hierarchy_filename",
    ] {
        assert!(
            install_properties.contains_key(property),
            "missing install property {property}"
        );
    }
}

#[test]
fn canonical_android_dynamic_tool_ignores_non_android_tools() {
    let tool = canonical_android_dynamic_tool(&DynamicToolSpec {
        namespace: None,
        name: "other_tool".to_string(),
        description: "other".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: false,
        persist_on_resume: true,
        capability: None,
    });

    assert!(tool.is_none());
}

#[test]
fn canonical_android_dynamic_tool_ignores_namespaced_android_names() {
    let tool = canonical_android_dynamic_tool(&DynamicToolSpec {
        namespace: Some("codex_app".to_string()),
        name: ANDROID_OBSERVE_TOOL_NAME.to_string(),
        description: "custom namespaced observe".to_string(),
        input_schema: json!({ "type": "object" }),
        defer_loading: false,
        persist_on_resume: true,
        capability: None,
    });

    assert!(tool.is_none());
}
