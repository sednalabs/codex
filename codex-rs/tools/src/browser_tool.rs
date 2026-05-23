use crate::JsonSchema;
use crate::ResponsesApiTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub const BROWSER_OBSERVE_TOOL_NAME: &str = "browser_observe";
pub const BROWSER_STEP_TOOL_NAME: &str = "browser_step";

const OBSERVE_SCOPE_VIEWPORT: &str = "viewport";
const OBSERVE_SCOPE_VIEWPORT_AND_PAGE: &str = "viewport_and_page";
const BACKEND_AUTO: &str = "auto";
const BACKEND_IAB: &str = "iab";
const BACKEND_CHROME: &str = "chrome";

const STEP_ACTIONS: [&str; 9] = [
    "navigate", "click", "type", "keypress", "scroll", "wait", "select", "drag", "hover",
];

pub fn canonical_browser_dynamic_tool(tool: &DynamicToolSpec) -> Option<ResponsesApiTool> {
    if tool.namespace.is_some() {
        return None;
    }

    match tool.name.as_str() {
        BROWSER_OBSERVE_TOOL_NAME => Some(create_browser_observe_tool(tool.defer_loading)),
        BROWSER_STEP_TOOL_NAME => Some(create_browser_step_tool(tool.defer_loading)),
        _ => None,
    }
}

fn create_browser_observe_tool(defer_loading: bool) -> ResponsesApiTool {
    let properties = BTreeMap::from([
        ("backend".to_string(), backend_schema()),
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
                &[OBSERVE_SCOPE_VIEWPORT, OBSERVE_SCOPE_VIEWPORT_AND_PAGE],
                "Whether to capture only the browser viewport or pair it with compact page metadata.",
            ),
        ),
    ]);

    ResponsesApiTool {
        name: BROWSER_OBSERVE_TOOL_NAME.to_string(),
        description:
            "Capture the current browser viewport as a model-visible screenshot, optionally with compact page metadata.".to_string(),
        strict: false,
        defer_loading: defer_loading.then_some(true),
        parameters: JsonSchema::object(properties, /*required*/ None, Some(false.into())),
        output_schema: None,
    }
}

fn create_browser_step_tool(defer_loading: bool) -> ResponsesApiTool {
    let action_item_properties = step_action_properties(/*include_type*/ true);
    let mut properties = step_action_properties(/*include_type*/ false);
    properties.insert("backend".to_string(), backend_schema());
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
                "Preferred batched browser action list. Execute actions in order before observing again.".to_string(),
            ),
        ),
    );
    properties.insert(
        "post_observe_scope".to_string(),
        string_enum(
            &[OBSERVE_SCOPE_VIEWPORT, OBSERVE_SCOPE_VIEWPORT_AND_PAGE],
            "Whether the post-action observation should include compact page metadata.",
        ),
    );
    properties.insert(
        "view".to_string(),
        view_schema(Some(
            "Optional persisted viewport metadata for zoomed or cropped follow-up actions."
                .to_string(),
        )),
    );

    ResponsesApiTool {
        name: BROWSER_STEP_TOOL_NAME.to_string(),
        description:
            "Perform one or more bounded browser actions, then return a fresh post-action viewport screenshot, summary, and current view metadata.".to_string(),
        strict: false,
        defer_loading: defer_loading.then_some(true),
        parameters: JsonSchema::object(properties, /*required*/ None, Some(false.into())),
        output_schema: None,
    }
}

fn step_action_properties(include_type: bool) -> BTreeMap<String, JsonSchema> {
    let mut properties = BTreeMap::from([
        (
            "url".to_string(),
            JsonSchema::string(Some("URL for navigate actions.".to_string())),
        ),
        (
            "selector".to_string(),
            permissive_object(Some(
                "Optional opaque selector object for selector-backed interactions.".to_string(),
            )),
        ),
        (
            "text".to_string(),
            JsonSchema::string(Some(
                "Text to type into the focused field or target element.".to_string(),
            )),
        ),
        (
            "x".to_string(),
            JsonSchema::number(Some(
                "X coordinate in the current browser viewport.".to_string(),
            )),
        ),
        (
            "y".to_string(),
            JsonSchema::number(Some(
                "Y coordinate in the current browser viewport.".to_string(),
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
            "key".to_string(),
            JsonSchema::string(Some("Key or key combination to send.".to_string())),
        ),
        (
            "keys".to_string(),
            JsonSchema::array(
                JsonSchema::string(Some("One key in a key combination.".to_string())),
                Some("Key combination to send.".to_string()),
            ),
        ),
        (
            "ms".to_string(),
            JsonSchema::integer(Some("Milliseconds to wait.".to_string())),
        ),
        (
            "timeout_secs".to_string(),
            JsonSchema::integer(Some(
                "Optional provider-side timeout in seconds for the action and observation."
                    .to_string(),
            )),
        ),
        (
            "tab_id".to_string(),
            JsonSchema::string(Some(
                "Optional opaque browser tab identifier supplied by the provider.".to_string(),
            )),
        ),
    ]);

    if include_type {
        properties.insert(
            "type".to_string(),
            string_enum(
                &STEP_ACTIONS,
                "Browser action to execute before the post-action observation.",
            ),
        );
    }

    properties
}

fn view_schema(description: Option<String>) -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            (
                "viewportWidth".to_string(),
                JsonSchema::number(Some("Browser viewport width in pixels.".to_string())),
            ),
            (
                "viewportHeight".to_string(),
                JsonSchema::number(Some("Browser viewport height in pixels.".to_string())),
            ),
            (
                "frame".to_string(),
                permissive_object(Some(
                    "Opaque frame metadata returned by a previous browser observation.".to_string(),
                )),
            ),
            (
                "region".to_string(),
                permissive_object(Some(
                    "Optional viewport crop or zoom region metadata.".to_string(),
                )),
            ),
            (
                "zoomed".to_string(),
                JsonSchema::boolean(Some(
                    "Whether the current view metadata describes a zoomed region.".to_string(),
                )),
            ),
        ]),
        /*required*/ None,
        Some(false.into()),
    );
    schema.description = description;
    schema
}

fn backend_schema() -> JsonSchema {
    string_enum(
        &[BACKEND_AUTO, BACKEND_IAB, BACKEND_CHROME],
        "Preferred browser provider backend. Use auto unless the task requires the in-app browser or signed-in Chrome state.",
    )
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
#[path = "browser_tool_tests.rs"]
mod tests;
