use crate::JsonSchema;
use crate::ResponsesApiTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use serde_json::json;
use std::collections::BTreeMap;

pub const ANDROID_OBSERVE_TOOL_NAME: &str = "android_observe";
pub const ANDROID_STEP_TOOL_NAME: &str = "android_step";
pub const ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME: &str = "android_install_build_from_run";

const OBSERVE_SCOPE_SCREEN: &str = "screen";
const OBSERVE_SCOPE_SCREEN_AND_UI: &str = "screen_and_ui";

const STEP_ACTIONS: [&str; 17] = [
    "launch_app",
    "tap",
    "type_text",
    "key",
    "swipe",
    "click",
    "double_click",
    "scroll",
    "type",
    "wait",
    "keypress",
    "drag",
    "long_press",
    "move",
    "zoom",
    "reset_zoom",
    "semantic_action",
];

pub fn canonical_android_dynamic_tool(tool: &DynamicToolSpec) -> Option<ResponsesApiTool> {
    if tool.namespace.is_some() {
        return None;
    }

    match tool.name.as_str() {
        ANDROID_OBSERVE_TOOL_NAME => Some(create_android_observe_tool(tool.defer_loading)),
        ANDROID_STEP_TOOL_NAME => Some(create_android_step_tool(tool.defer_loading)),
        ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME => Some(
            create_android_install_build_from_run_tool(tool.defer_loading),
        ),
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
        (
            "serial".to_string(),
            JsonSchema::string(Some(
                "Optional Android device serial to observe.".to_string(),
            )),
        ),
        (
            "timeout_secs".to_string(),
            JsonSchema::number(Some(
                "Optional provider-side timeout in seconds for screenshot and UI-tree capture."
                    .to_string(),
            )),
        ),
        (
            "screenshot_filename".to_string(),
            JsonSchema::string(Some(
                "Optional provider artifact filename for the screenshot receipt. The provider must still return native image content for model-visible screenshots.".to_string(),
            )),
        ),
        (
            "hierarchy_filename".to_string(),
            JsonSchema::string(Some(
                "Optional provider artifact filename for the UI hierarchy receipt.".to_string(),
            )),
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
        "screenshot_filename".to_string(),
        JsonSchema::string(Some(
            "Optional provider artifact filename for the post-action screenshot receipt. The provider must still return native image content for model-visible screenshots.".to_string(),
        )),
    );
    properties.insert(
        "hierarchy_filename".to_string(),
        JsonSchema::string(Some(
            "Optional provider artifact filename for the post-action UI hierarchy receipt."
                .to_string(),
        )),
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

fn create_android_install_build_from_run_tool(defer_loading: bool) -> ResponsesApiTool {
    let properties = BTreeMap::from([
        (
            "workflow_run_id".to_string(),
            JsonSchema::integer(Some(
                "GitHub Actions workflow run id that produced the Android build artifact."
                    .to_string(),
            )),
        ),
        (
            "artifact_name".to_string(),
            JsonSchema::string(Some(
                "Name of the workflow artifact containing the Android build bundle.".to_string(),
            )),
        ),
        (
            "repository".to_string(),
            JsonSchema::string(Some(
                "Optional owner/repo override. The provider default is used when omitted."
                    .to_string(),
            )),
        ),
        (
            "serial".to_string(),
            JsonSchema::string(Some(
                "Optional Android device serial to target.".to_string(),
            )),
        ),
        (
            "launch_after_install".to_string(),
            JsonSchema::boolean(Some(
                "Whether to launch the installed build after install. Defaults to true."
                    .to_string(),
            )),
        ),
        (
            "timeout_secs".to_string(),
            JsonSchema::integer(Some(
                "Optional provider-side timeout in seconds for install, launch, and postcondition checks."
                    .to_string(),
            )),
        ),
        (
            "post_observe_scope".to_string(),
            string_enum(
                &[OBSERVE_SCOPE_SCREEN, OBSERVE_SCOPE_SCREEN_AND_UI],
                "Whether the post-install observation should include a compact UI digest.",
            ),
        ),
        (
            "screenshot_filename".to_string(),
            JsonSchema::string(Some(
                "Optional provider artifact filename for the post-install screenshot receipt. The provider must still return native image content for model-visible screenshots.".to_string(),
            )),
        ),
        (
            "hierarchy_filename".to_string(),
            JsonSchema::string(Some(
                "Optional provider artifact filename for the post-install UI hierarchy receipt."
                    .to_string(),
            )),
        ),
    ]);

    ResponsesApiTool {
        name: ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME.to_string(),
        description:
            "Install a GitHub Actions Android build artifact into the active Android session, optionally launch it, then return a post-install observation when available.".to_string(),
        strict: false,
        defer_loading: defer_loading.then_some(true),
        parameters: JsonSchema::object(
            properties,
            Some(vec![
                "workflow_run_id".to_string(),
                "artifact_name".to_string(),
            ]),
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
            "package".to_string(),
            JsonSchema::string(Some(
                "Compatibility alias for package_name.".to_string(),
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
            android_selector_schema(Some(
                "Optional selector for UI-tree-backed interactions. Prefer text, content description, resource id, class name, or bounds-derived targets over visually guessed coordinates.".to_string(),
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
            "key".to_string(),
            JsonSchema::string(Some("Compatibility alias for keycode.".to_string())),
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
            "ms".to_string(),
            JsonSchema::number(Some("Wait duration in milliseconds for wait actions.".to_string())),
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
            android_selector_schema(Some(
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
            android_selector_schema(Some(
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
            android_selector_schema(Some(
                "Optional selector-like target payload for semantic or element actions.".to_string(),
            )),
        ),
        (
            "region".to_string(),
            region_schema(Some("Zoom region for zoom actions.".to_string())),
        ),
        (
            "frame".to_string(),
            frame_schema(Some(
                "Optional frame metadata used to interpret view-space coordinates.".to_string(),
            )),
        ),
        (
            "frameWidth".to_string(),
            JsonSchema::number(Some("Compatibility field for frame width.".to_string())),
        ),
        (
            "frameHeight".to_string(),
            JsonSchema::number(Some("Compatibility field for frame height.".to_string())),
        ),
        (
            "frame_width".to_string(),
            JsonSchema::number(Some("Compatibility field for frame width.".to_string())),
        ),
        (
            "frame_height".to_string(),
            JsonSchema::number(Some("Compatibility field for frame height.".to_string())),
        ),
    ]);

    if include_type {
        properties.insert(
            "type".to_string(),
            string_enum(
                &STEP_ACTIONS,
                "Android action type. Accepts legacy Android action names and computer-style action names.",
            ),
        );
    }

    properties
}

fn view_schema(description: Option<String>) -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            (
                "deviceWidth".to_string(),
                JsonSchema::number(Some("Device pixel width.".to_string())),
            ),
            (
                "deviceHeight".to_string(),
                JsonSchema::number(Some("Device pixel height.".to_string())),
            ),
            (
                "device_width".to_string(),
                JsonSchema::number(Some(
                    "Compatibility field for device pixel width.".to_string(),
                )),
            ),
            (
                "device_height".to_string(),
                JsonSchema::number(Some(
                    "Compatibility field for device pixel height.".to_string(),
                )),
            ),
            (
                "device".to_string(),
                JsonSchema::object(
                    BTreeMap::from([
                        (
                            "width".to_string(),
                            JsonSchema::number(Some("Device pixel width.".to_string())),
                        ),
                        (
                            "height".to_string(),
                            JsonSchema::number(Some("Device pixel height.".to_string())),
                        ),
                    ]),
                    /*required*/ None,
                    Some(false.into()),
                ),
            ),
            (
                "frameWidth".to_string(),
                JsonSchema::number(Some("Current frame width.".to_string())),
            ),
            (
                "frameHeight".to_string(),
                JsonSchema::number(Some("Current frame height.".to_string())),
            ),
            (
                "frame_width".to_string(),
                JsonSchema::number(Some(
                    "Compatibility field for current frame width.".to_string(),
                )),
            ),
            (
                "frame_height".to_string(),
                JsonSchema::number(Some(
                    "Compatibility field for current frame height.".to_string(),
                )),
            ),
            (
                "frame".to_string(),
                frame_schema(Some("Current frame dimensions.".to_string())),
            ),
            (
                "region".to_string(),
                region_schema(Some(
                    "Current zoom or crop region in device coordinates.".to_string(),
                )),
            ),
            (
                "zoomed".to_string(),
                JsonSchema::boolean(Some(
                    "Whether the current view is zoomed or cropped.".to_string(),
                )),
            ),
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

fn frame_schema(description: Option<String>) -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            (
                "width".to_string(),
                JsonSchema::number(Some("Frame width.".to_string())),
            ),
            (
                "height".to_string(),
                JsonSchema::number(Some("Frame height.".to_string())),
            ),
        ]),
        /*required*/ None,
        Some(false.into()),
    );
    schema.description = description;
    schema
}

fn region_schema(description: Option<String>) -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            (
                "left".to_string(),
                JsonSchema::number(Some("Left coordinate in device space.".to_string())),
            ),
            (
                "top".to_string(),
                JsonSchema::number(Some("Top coordinate in device space.".to_string())),
            ),
            (
                "width".to_string(),
                JsonSchema::number(Some("Region width in device space.".to_string())),
            ),
            (
                "height".to_string(),
                JsonSchema::number(Some("Region height in device space.".to_string())),
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

fn android_selector_schema(description: Option<String>) -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            (
                "text".to_string(),
                JsonSchema::string(Some(
                    "Visible text to match in the Android UI tree.".to_string(),
                )),
            ),
            (
                "content_description".to_string(),
                JsonSchema::string(Some(
                    "Accessibility content description to match.".to_string(),
                )),
            ),
            (
                "contentDescription".to_string(),
                JsonSchema::string(Some(
                    "Compatibility alias for content_description.".to_string(),
                )),
            ),
            (
                "resource_id".to_string(),
                JsonSchema::string(Some(
                    "Android resource id to match, when exposed by the UI tree.".to_string(),
                )),
            ),
            (
                "resourceId".to_string(),
                JsonSchema::string(Some(
                    "Compatibility alias for resource_id.".to_string(),
                )),
            ),
            (
                "class_name".to_string(),
                JsonSchema::string(Some(
                    "Android view class name to match when text/id are insufficient.".to_string(),
                )),
            ),
            (
                "className".to_string(),
                JsonSchema::string(Some("Compatibility alias for class_name.".to_string())),
            ),
            (
                "bounds".to_string(),
                bounds_schema(Some(
                    "Device-space bounds from a UI-tree node; providers should tap the center when using bounds as a target.".to_string(),
                )),
            ),
            (
                "enabled".to_string(),
                JsonSchema::boolean(Some(
                    "Optional expected enabled state for candidate filtering.".to_string(),
                )),
            ),
        ]),
        /*required*/ None,
        Some(true.into()),
    );
    schema.description = description;
    schema
}

fn bounds_schema(description: Option<String>) -> JsonSchema {
    let mut schema = JsonSchema::object(
        BTreeMap::from([
            (
                "left".to_string(),
                JsonSchema::number(Some("Left device-space coordinate.".to_string())),
            ),
            (
                "top".to_string(),
                JsonSchema::number(Some("Top device-space coordinate.".to_string())),
            ),
            (
                "right".to_string(),
                JsonSchema::number(Some("Right device-space coordinate.".to_string())),
            ),
            (
                "bottom".to_string(),
                JsonSchema::number(Some("Bottom device-space coordinate.".to_string())),
            ),
            (
                "width".to_string(),
                JsonSchema::number(Some("Optional bounds width in device pixels.".to_string())),
            ),
            (
                "height".to_string(),
                JsonSchema::number(Some("Optional bounds height in device pixels.".to_string())),
            ),
        ]),
        /*required*/ None,
        Some(false.into()),
    );
    schema.description = description;
    schema
}

#[cfg(test)]
#[path = "android_tool_tests.rs"]
mod tests;
