use super::augment_tool_spec_for_code_mode;
use super::tool_spec_to_code_mode_tool_definition;
use crate::AdditionalProperties;
use crate::FreeformTool;
use crate::FreeformToolFormat;
use crate::JsonSchema;
use crate::ResponsesApiTool;
use crate::ToolSpec;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;

#[test]
fn augment_tool_spec_for_code_mode_augments_function_tools() {
    assert_eq!(
        augment_tool_spec_for_code_mode(ToolSpec::Function(ResponsesApiTool {
            name: "lookup_order".to_string(),
            description: "Look up an order".to_string(),
            strict: false,
            defer_loading: Some(true),
            parameters: JsonSchema::Object {
                properties: BTreeMap::from([(
                    "order_id".to_string(),
                    JsonSchema::String { description: None },
                )]),
                required: Some(vec!["order_id".to_string()]),
                additional_properties: Some(AdditionalProperties::Boolean(false)),
            },
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "ok": {"type": "boolean"}
                },
                "required": ["ok"],
            })),
        })),
        ToolSpec::Function(ResponsesApiTool {
            name: "lookup_order".to_string(),
            description: "Look up an order\n\nexec tool declaration:\n```ts\ndeclare const tools: { lookup_order(args: { order_id: string; }): Promise<{ ok: boolean; }>; };\n```".to_string(),
            strict: false,
            defer_loading: Some(true),
            parameters: JsonSchema::Object {
                properties: BTreeMap::from([(
                    "order_id".to_string(),
                    JsonSchema::String { description: None },
                )]),
                required: Some(vec!["order_id".to_string()]),
                additional_properties: Some(AdditionalProperties::Boolean(false)),
            },
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "ok": {"type": "boolean"}
                },
                "required": ["ok"],
            })),
        })
    );
}

#[test]
fn augment_tool_spec_for_code_mode_preserves_exec_tool_description() {
    assert_eq!(
        augment_tool_spec_for_code_mode(ToolSpec::Freeform(FreeformTool {
            name: codex_code_mode::PUBLIC_TOOL_NAME.to_string(),
            description: "Run code".to_string(),
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: \"exec\"".to_string(),
            },
        })),
        ToolSpec::Freeform(FreeformTool {
            name: codex_code_mode::PUBLIC_TOOL_NAME.to_string(),
            description: "Run code".to_string(),
            format: FreeformToolFormat {
                r#type: "grammar".to_string(),
                syntax: "lark".to_string(),
                definition: "start: \"exec\"".to_string(),
            },
        })
    );
}

#[test]
fn tool_spec_to_code_mode_tool_definition_returns_augmented_nested_tools() {
    let spec = ToolSpec::Freeform(FreeformTool {
        name: "apply_patch".to_string(),
        description: "Apply a patch".to_string(),
        format: FreeformToolFormat {
            r#type: "grammar".to_string(),
            syntax: "lark".to_string(),
            definition: "start: \"patch\"".to_string(),
        },
    });

    assert_eq!(
        tool_spec_to_code_mode_tool_definition(&spec),
        Some(codex_code_mode::ToolDefinition {
            name: "apply_patch".to_string(),
            all_tools_name: None,
            all_tools_module: None,
            description: "Apply a patch\n\nexec tool declaration:\n```ts\ndeclare const tools: { apply_patch(input: string): Promise<unknown>; };\n```".to_string(),
            kind: codex_code_mode::CodeModeToolKind::Freeform,
            input_schema: None,
            output_schema: None,
        })
    );
}

#[test]
fn tool_spec_to_code_mode_tool_definition_preserves_mcp_module_metadata() {
    let spec = ToolSpec::Function(ResponsesApiTool {
        name: "mcp__rmcp__echo".to_string(),
        description: "Echo text".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties: BTreeMap::from([(
                "message".to_string(),
                JsonSchema::String { description: None },
            )]),
            required: Some(vec!["message".to_string()]),
            additional_properties: Some(AdditionalProperties::Boolean(false)),
        },
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"}
            },
            "required": ["ok"],
        })),
    });

    assert_eq!(
        tool_spec_to_code_mode_tool_definition(&spec),
        Some(codex_code_mode::ToolDefinition {
            name: "mcp__rmcp__echo".to_string(),
            all_tools_name: Some("echo".to_string()),
            all_tools_module: Some("tools/mcp/rmcp.js".to_string()),
            description: "Echo text\n\nexec tool declaration:\n```ts\ndeclare const tools: { mcp__rmcp__echo(args: { message: string; }): Promise<{ ok: boolean; }>; };\n```".to_string(),
            kind: codex_code_mode::CodeModeToolKind::Function,
            input_schema: Some(json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string"}
                },
                "required": ["message"],
                "additionalProperties": false
            })),
            output_schema: Some(json!({
                "type": "object",
                "properties": {
                    "ok": {"type": "boolean"}
                },
                "required": ["ok"],
            })),
        })
    );
}

#[test]
fn tool_spec_to_code_mode_tool_definition_skips_unsupported_variants() {
    assert_eq!(
        tool_spec_to_code_mode_tool_definition(&ToolSpec::ToolSearch {
            execution: "sync".to_string(),
            description: "Search".to_string(),
            parameters: JsonSchema::Object {
                properties: BTreeMap::new(),
                required: None,
                additional_properties: None,
            },
        }),
        None
    );
}
