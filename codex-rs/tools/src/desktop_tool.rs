use crate::JsonSchema;
use crate::ResponsesApiTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub const DESKTOP_OBSERVE_TOOL_NAME: &str = "desktop_observe";
pub const DESKTOP_STEP_TOOL_NAME: &str = "desktop_step";

const OBSERVE_SCOPE_SCREEN: &str = "screen";
const OBSERVE_SCOPE_SCREEN_AND_UI: &str = "screen_and_ui";

const STEP_ACTIONS: [&str; 9] = [
    "click",
    "type_text",
    "press_key",
    "scroll",
    "drag",
    "set_value",
    "select_text",
    "wait",
    "move",
];

pub fn canonical_desktop_dynamic_tool(tool: &DynamicToolSpec) -> Option<ResponsesApiTool> {
    if tool.namespace.is_some() {
        return None;
    }

    match tool.name.as_str() {
        DESKTOP_OBSERVE_TOOL_NAME => Some(create_desktop_observe_tool(tool.defer_loading)),
        DESKTOP_STEP_TOOL_NAME => Some(create_desktop_step_tool(tool.defer_loading)),
        _ => None,
    }
}

fn create_desktop_observe_tool(defer_loading: bool) -> ResponsesApiTool {
    let properties = BTreeMap::from([
        (
            "prompt".to_string(),
            JsonSchema::string(Some(
                "Optional observation focus hint describing which app or UI region to inspect."
                    .to_string(),
            )),
        ),
        (
            "scope".to_string(),
            string_enum(
                &[OBSERVE_SCOPE_SCREEN, OBSERVE_SCOPE_SCREEN_AND_UI],
                "Whether to capture only the screenshot or pair it with a compact accessibility/UI digest.",
            ),
        ),
        (
            "app".to_string(),
            JsonSchema::string(Some(
                "Optional provider-specific app name or bundle identifier to focus before observation."
                    .to_string(),
            )),
        ),
    ]);

    ResponsesApiTool {
        name: DESKTOP_OBSERVE_TOOL_NAME.to_string(),
        description:
            "Capture the current desktop app state as a model-visible screenshot, optionally with a compact accessibility/UI digest."
                .to_string(),
        strict: false,
        defer_loading: defer_loading.then_some(true),
        parameters: JsonSchema::object(properties, /*required*/ None, Some(false.into())),
        output_schema: None,
    }
}

fn create_desktop_step_tool(defer_loading: bool) -> ResponsesApiTool {
    let action_item_properties = step_action_properties(/*include_type*/ true);
    let mut properties = step_action_properties(/*include_type*/ false);
    properties.insert(
        "action".to_string(),
        string_enum(
            &STEP_ACTIONS,
            "Single-action compatibility field. Prefer actions[] for new calls.",
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
                "Preferred batched desktop action list. Execute actions in order before observing again."
                    .to_string(),
            ),
        ),
    );
    properties.insert(
        "post_observe_scope".to_string(),
        string_enum(
            &[OBSERVE_SCOPE_SCREEN, OBSERVE_SCOPE_SCREEN_AND_UI],
            "Whether the post-action observation should include a compact accessibility/UI digest.",
        ),
    );
    properties.insert(
        "view".to_string(),
        view_schema(Some(
            "Optional persisted desktop view metadata from a previous observation.".to_string(),
        )),
    );

    ResponsesApiTool {
        name: DESKTOP_STEP_TOOL_NAME.to_string(),
        description:
            "Perform one or more bounded desktop UI actions, then return a fresh post-action screenshot, summary, and current view metadata."
                .to_string(),
        strict: false,
        defer_loading: defer_loading.then_some(true),
        parameters: JsonSchema::object(properties, /*required*/ None, Some(false.into())),
        output_schema: None,
    }
}

fn step_action_properties(include_type: bool) -> BTreeMap<String, JsonSchema> {
    let mut properties = BTreeMap::from([
        (
            "app".to_string(),
            JsonSchema::string(Some(
                "Optional provider-specific app name or bundle identifier.".to_string(),
            )),
        ),
        (
            "window_id".to_string(),
            JsonSchema::string(Some(
                "Optional opaque window identifier returned by the provider.".to_string(),
            )),
        ),
        (
            "element_id".to_string(),
            JsonSchema::string(Some(
                "Optional opaque accessibility/UI element identifier returned by the provider."
                    .to_string(),
            )),
        ),
        (
            "element_index".to_string(),
            JsonSchema::integer(Some(
                "Compatibility field for an indexed element in the provider's UI digest."
                    .to_string(),
            )),
        ),
        (
            "selector".to_string(),
            permissive_object(Some(
                "Optional opaque selector object for provider-specific UI targeting.".to_string(),
            )),
        ),
        (
            "text".to_string(),
            JsonSchema::string(Some(
                "Text to type, set, or select depending on action type.".to_string(),
            )),
        ),
        (
            "target_text".to_string(),
            JsonSchema::string(Some(
                "Exact visible text from the accessibility/UI digest for select_text actions."
                    .to_string(),
            )),
        ),
        (
            "x".to_string(),
            JsonSchema::number(Some(
                "X coordinate in screenshot or view coordinates.".to_string(),
            )),
        ),
        (
            "y".to_string(),
            JsonSchema::number(Some(
                "Y coordinate in screenshot or view coordinates.".to_string(),
            )),
        ),
        (
            "x1".to_string(),
            JsonSchema::number(Some("Start X coordinate for drag actions.".to_string())),
        ),
        (
            "y1".to_string(),
            JsonSchema::number(Some("Start Y coordinate for drag actions.".to_string())),
        ),
        (
            "x2".to_string(),
            JsonSchema::number(Some("End X coordinate for drag actions.".to_string())),
        ),
        (
            "y2".to_string(),
            JsonSchema::number(Some("End Y coordinate for drag actions.".to_string())),
        ),
        (
            "button".to_string(),
            string_enum(
                &["left", "right", "middle"],
                "Mouse button for click actions. Defaults to left.",
            ),
        ),
        (
            "click_count".to_string(),
            JsonSchema::integer(Some("Number of clicks for click actions.".to_string())),
        ),
        (
            "scroll_x".to_string(),
            JsonSchema::number(Some(
                "Horizontal scroll delta for scroll actions.".to_string(),
            )),
        ),
        (
            "scroll_y".to_string(),
            JsonSchema::number(Some(
                "Vertical scroll delta for scroll actions.".to_string(),
            )),
        ),
        (
            "pages".to_string(),
            JsonSchema::number(Some(
                "Provider-specific page count for page-based scroll actions.".to_string(),
            )),
        ),
        (
            "key".to_string(),
            JsonSchema::string(Some("Key or key combination to press.".to_string())),
        ),
        (
            "keys".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("One key in a key combination.".to_string())),
                Some("Key combination to press.".to_string()),
            ),
        ),
        (
            "ms".to_string(),
            JsonSchema::integer(Some("Milliseconds to wait for wait actions.".to_string())),
        ),
        (
            "timeout_secs".to_string(),
            JsonSchema::integer(Some(
                "Optional provider-side timeout in seconds for the action and observation."
                    .to_string(),
            )),
        ),
    ]);

    if include_type {
        properties.insert(
            "type".to_string(),
            string_enum(
                &STEP_ACTIONS,
                "Desktop action to execute before the post-action observation.",
            ),
        );
    }

    properties
}

fn view_schema(description: Option<String>) -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            (
                "screenWidth".to_string(),
                JsonSchema::number(Some(
                    "Captured screen or appshot width in pixels.".to_string(),
                )),
            ),
            (
                "screenHeight".to_string(),
                JsonSchema::number(Some(
                    "Captured screen or appshot height in pixels.".to_string(),
                )),
            ),
            (
                "app".to_string(),
                JsonSchema::string(Some(
                    "Provider-specific app name or bundle identifier.".to_string(),
                )),
            ),
            (
                "window".to_string(),
                permissive_object(Some(
                    "Opaque provider window metadata from a previous observation.".to_string(),
                )),
            ),
            (
                "region".to_string(),
                permissive_object(Some(
                    "Optional screenshot crop, zoom, or appshot region metadata.".to_string(),
                )),
            ),
        ]),
        /*required*/ None,
        Some(false.into()),
    );
    schema.description = description;
    schema
}

fn permissive_object(description: Option<String>) -> JsonSchema {
    let mut schema = JsonSchema::object(BTreeMap::new(), /*required*/ None, Some(true.into()));
    schema.description = description;
    schema
}

fn string_enum(values: &[&str], description: &str) -> JsonSchema {
    JsonSchema::string_enum(
        values.iter().map(|value| json!(*value)).collect::<Vec<_>>(),
        Some(description.to_string()),
    )
}

#[cfg(test)]
#[path = "desktop_tool_tests.rs"]
mod tests;
