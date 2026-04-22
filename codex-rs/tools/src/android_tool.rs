use crate::JsonSchema;
use crate::ResponsesApiTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub const ANDROID_OBSERVE_TOOL_NAME: &str = "android_observe";
pub const ANDROID_STEP_TOOL_NAME: &str = "android_step";

const OBSERVE_SCOPE_SCREEN: &str = "screen";
const OBSERVE_SCOPE_SCREEN_AND_UI: &str = "screen_and_ui";

const STEP_ACTIONS: [&str; 12] = [
    "launch_app",
    "click",
    "double_click",
    "scroll",
    "type",
    "wait",
    "keypress",
    "drag",
    "move",
    "zoom",
    "reset_zoom",
    "semantic_action",
];

pub fn canonical_android_dynamic_tool(tool: &DynamicToolSpec) -> Option<ResponsesApiTool> {
    match tool.name.as_str() {
        ANDROID_OBSERVE_TOOL_NAME => Some(create_android_observe_tool(tool.defer_loading)),
        ANDROID_STEP_TOOL_NAME => Some(create_android_step_tool(tool.defer_loading)),
        _ => None,
    }
}

fn create_android_observe_tool(defer_loading: bool) -> ResponsesApiTool {
    let properties = BTreeMap::from([
        (
            "prompt".to_string(),
            JsonSchema::string(Some(
                "Optional observation focus hint describing what the model should inspect."
                    .to_string(),
            )),
        ),
        (
            "scope".to_string(),
            string_enum(
                &[OBSERVE_SCOPE_SCREEN, OBSERVE_SCOPE_SCREEN_AND_UI],
                "Whether to capture only the screenshot or pair it with a compact UI digest.",
            ),
        ),
    ]);

    ResponsesApiTool {
        name: ANDROID_OBSERVE_TOOL_NAME.to_string(),
        description:
            "Capture the current Android screen as a model-visible screenshot, optionally with a compact UI digest.".to_string(),
        strict: false,
        defer_loading: defer_loading.then_some(true),
        parameters: JsonSchema::object(
            properties,
            /*required*/ None,
            Some(false.into()),
        ),
        output_schema: None,
    }
}

fn create_android_step_tool(defer_loading: bool) -> ResponsesApiTool {
    let action_item_properties = step_action_properties(/*include_type*/ true);
    let mut properties = step_action_properties(/*include_type*/ false);
    properties.insert(
        "action".to_string(),
        string_enum(
            &STEP_ACTIONS,
            "Legacy single-action compatibility field. Prefer actions[] for new calls.",
        ),
    );
    properties.insert(
        "actions".to_string(),
        JsonSchema::array(
            JsonSchema::object(
                action_item_properties,
                Some(vec!["type".to_string()]),
                Some(false.into()),
            ),
            Some(
                "Preferred batched Android action list. Execute actions in order before observing again.".to_string(),
            ),
        ),
    );
    properties.insert(
        "post_observe_scope".to_string(),
        string_enum(
            &[OBSERVE_SCOPE_SCREEN, OBSERVE_SCOPE_SCREEN_AND_UI],
            "Whether the post-action observation should include a compact UI digest.",
        ),
    );
    properties.insert(
        "view".to_string(),
        view_schema(Some(
            "Optional persisted view metadata for zoomed or cropped follow-up actions.".to_string(),
        )),
    );

    ResponsesApiTool {
        name: ANDROID_STEP_TOOL_NAME.to_string(),
        description:
            "Perform one or more bounded Android actions, then return a fresh post-action screenshot, summary, and current view metadata.".to_string(),
        strict: false,
        defer_loading: defer_loading.then_some(true),
        parameters: JsonSchema::object(
            properties,
            /*required*/ None,
            Some(false.into()),
        ),
        output_schema: None,
    }
}

fn step_action_properties(include_type: bool) -> BTreeMap<String, JsonSchema> {
    let mut properties = BTreeMap::from([
        (
            "package_name".to_string(),
            JsonSchema::string(Some(
                "Android package name to launch or reuse as the default package for this step."
                    .to_string(),
            )),
        ),
        (
            "activity".to_string(),
            JsonSchema::string(Some(
                "Optional Android activity to launch or reuse as the default activity for this step."
                    .to_string(),
            )),
        ),
        (
            "selector".to_string(),
            permissive_object(Some(
                "Optional opaque selector object for selector-backed interactions.".to_string(),
            )),
        ),
        (
            "text".to_string(),
            JsonSchema::string(Some("Text to type into the focused field or target element.".to_string())),
        ),
        ("x".to_string(), JsonSchema::number(Some("X coordinate in the current view.".to_string()))),
        ("y".to_string(), JsonSchema::number(Some("Y coordinate in the current view.".to_string()))),
        (
            "x1".to_string(),
            JsonSchema::number(Some("Start X coordinate for drag or swipe actions.".to_string())),
        ),
        (
            "y1".to_string(),
            JsonSchema::number(Some("Start Y coordinate for drag or swipe actions.".to_string())),
        ),
        (
            "x2".to_string(),
            JsonSchema::number(Some("End X coordinate for drag or swipe actions.".to_string())),
        ),
        (
            "y2".to_string(),
            JsonSchema::number(Some("End Y coordinate for drag or swipe actions.".to_string())),
        ),
        (
            "scroll_x".to_string(),
            JsonSchema::number(Some("Horizontal scroll delta for scroll actions.".to_string())),
        ),
        (
            "scroll_y".to_string(),
            JsonSchema::number(Some("Vertical scroll delta for scroll actions.".to_string())),
        ),
        (
            "keycode".to_string(),
            JsonSchema::string(Some("Legacy single-key compatibility field for keypress actions.".to_string())),
        ),
        (
            "keys".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("Key name or keycode.".to_string())),
                Some("Ordered key sequence for keypress actions.".to_string()),
            ),
        ),
        (
            "wait_ms".to_string(),
            JsonSchema::number(Some("Legacy compatibility field for wait duration in milliseconds.".to_string())),
        ),
        (
            "timeout_ms".to_string(),
            JsonSchema::number(Some("Optional timeout in milliseconds for compatible actions.".to_string())),
        ),
        (
            "timeout_secs".to_string(),
            JsonSchema::number(Some("Optional timeout in seconds for compatible actions.".to_string())),
        ),
        (
            "duration_ms".to_string(),
            JsonSchema::number(Some("Optional drag or swipe duration in milliseconds.".to_string())),
        ),
        (
            "name".to_string(),
            JsonSchema::string(Some("Semantic action name or compatibility alias.".to_string())),
        ),
        (
            "action_name".to_string(),
            JsonSchema::string(Some("Semantic action compatibility field.".to_string())),
        ),
        (
            "wait_for_selector".to_string(),
            permissive_object(Some(
                "Optional selector to wait for after the action completes.".to_string(),
            )),
        ),
        (
            "wait_for_activity".to_string(),
            JsonSchema::string(Some("Optional activity to wait for after the action.".to_string())),
        ),
        (
            "wait_for_package".to_string(),
            JsonSchema::string(Some("Optional package name to wait for after the action.".to_string())),
        ),
        (
            "expect_focus_selector".to_string(),
            permissive_object(Some(
                "Optional selector that should become focused after a type action.".to_string(),
            )),
        ),
        (
            "expect_scroll_change".to_string(),
            JsonSchema::boolean(Some(
                "Whether the action should verify that the scroll position changed.".to_string(),
            )),
        ),
        (
            "wait_until_absent".to_string(),
            JsonSchema::boolean(Some(
                "Whether wait_for_selector should wait for the selector to disappear.".to_string(),
            )),
        ),
        (
            "match_index".to_string(),
            JsonSchema::number(Some(
                "Optional zero-based match index for ambiguous selector results.".to_string(),
            )),
        ),
        (
            "target".to_string(),
            permissive_object(Some("Optional semantic action target payload.".to_string())),
        ),
    ]);

    if include_type {
        properties.insert(
            "type".to_string(),
            string_enum(
                &STEP_ACTIONS,
                "Android action type. Use actions[] for new batched calls.",
            ),
        );
    }

    properties
}

fn view_schema(description: Option<String>) -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            (
                "origin_x".to_string(),
                JsonSchema::number(Some(
                    "Original device-space X coordinate of the current cropped view.".to_string(),
                )),
            ),
            (
                "origin_y".to_string(),
                JsonSchema::number(Some(
                    "Original device-space Y coordinate of the current cropped view.".to_string(),
                )),
            ),
            (
                "width".to_string(),
                JsonSchema::number(Some("Current cropped view width in pixels.".to_string())),
            ),
            (
                "height".to_string(),
                JsonSchema::number(Some("Current cropped view height in pixels.".to_string())),
            ),
            (
                "scale".to_string(),
                JsonSchema::number(Some(
                    "Current zoom scale relative to device space.".to_string(),
                )),
            ),
        ]),
        /*required*/ None,
        Some(false.into()),
    );
    schema.description = description;
    schema
}

fn string_enum(values: &[&str], description: &str) -> JsonSchema {
    JsonSchema::string_enum(
        values.iter().map(|value| json!(value)).collect(),
        Some(description.to_string()),
    )
}

fn permissive_object(description: Option<String>) -> JsonSchema {
    let mut schema = JsonSchema::object(BTreeMap::new(), /*required*/ None, Some(true.into()));
    schema.description = description;
    schema
}

#[cfg(test)]
#[path = "android_tool_tests.rs"]
mod tests;
