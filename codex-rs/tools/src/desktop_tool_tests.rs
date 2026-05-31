use super::DESKTOP_OBSERVE_TOOL_NAME;
use super::DESKTOP_STEP_TOOL_NAME;
use super::canonical_desktop_dynamic_tool;
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
fn canonical_desktop_observe_schema_exposes_screen_and_ui_scope() {
    let observe = canonical_desktop_dynamic_tool(&dynamic_tool(DESKTOP_OBSERVE_TOOL_NAME))
        .expect("desktop observe should be native computer-use");

    assert_eq!(observe.name, DESKTOP_OBSERVE_TOOL_NAME);
    let parameters = serde_json::to_value(&observe.parameters).expect("parameters serialize");
    assert_eq!(
        parameters["properties"]["scope"]["enum"],
        json!(["screen", "screen_and_ui"])
    );
    assert_eq!(parameters["additionalProperties"], false);
}

#[test]
fn canonical_desktop_step_schema_exposes_cleanroom_action_set() {
    let step = canonical_desktop_dynamic_tool(&dynamic_tool(DESKTOP_STEP_TOOL_NAME))
        .expect("desktop step should be native computer-use");

    assert_eq!(step.name, DESKTOP_STEP_TOOL_NAME);
    let parameters = serde_json::to_value(&step.parameters).expect("parameters serialize");
    assert_eq!(
        parameters["properties"]["actions"]["items"]["required"],
        json!(["type"])
    );
    assert_eq!(
        parameters["properties"]["actions"]["items"]["properties"]["type"]["enum"],
        json!([
            "click",
            "type_text",
            "press_key",
            "scroll",
            "drag",
            "set_value",
            "select_text",
            "wait",
            "move"
        ])
    );
    assert_eq!(parameters["additionalProperties"], false);
}

#[test]
fn namespaced_desktop_tools_are_not_promoted() {
    let mut tool = dynamic_tool(DESKTOP_OBSERVE_TOOL_NAME);
    tool.namespace = Some("app".to_string());

    assert!(canonical_desktop_dynamic_tool(&tool).is_none());
}
