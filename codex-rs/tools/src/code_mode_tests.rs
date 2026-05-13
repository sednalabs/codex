use super::augment_tool_spec_for_code_mode;
use super::tool_spec_to_code_mode_tool_definition;
use crate::AdditionalProperties;
use crate::FreeformTool;
use crate::FreeformToolFormat;
use crate::JsonSchema;
use crate::ResponsesApiTool;
use crate::ToolName;
use crate::ToolSpec;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;

#[derive(Debug)]
struct ParsedCodeModeDeclaration {
    name: String,
    input_name: String,
    args: Vec<String>,
    output: Vec<String>,
}

#[derive(Clone, Copy, Default)]
struct CodeModeQuoteState {
    in_single_quoted: bool,
    in_double_quoted: bool,
    in_template_literal: bool,
    escaped: bool,
}

impl CodeModeQuoteState {
    fn in_quote(self) -> bool {
        self.in_single_quoted || self.in_double_quoted || self.in_template_literal
    }
}

fn advance_code_mode_quote_state(ch: char, state: &mut CodeModeQuoteState) {
    if state.escaped {
        state.escaped = false;
        return;
    }

    match ch {
        '\\' if state.in_quote() => {
            state.escaped = true;
        }
        '\'' if !state.in_double_quoted && !state.in_template_literal => {
            state.in_single_quoted = !state.in_single_quoted;
        }
        '"' if !state.in_single_quoted && !state.in_template_literal => {
            state.in_double_quoted = !state.in_double_quoted;
        }
        '`' if !state.in_single_quoted && !state.in_double_quoted => {
            state.in_template_literal = !state.in_template_literal;
        }
        _ => {}
    }
}

fn assert_code_mode_description(
    description: &str,
    prose: &str,
    name: &str,
    input_name: &str,
    arg_fields: &[&str],
    output_fields: &[&str],
) {
    let (actual_prose, _, trailing) = split_code_mode_description(description)
        .expect("description should match code-mode description shape");
    assert_eq!(actual_prose, prose);
    assert_eq!(trailing, "");
    assert_code_mode_declaration_fields(description, name, input_name, arg_fields, output_fields);
}

fn assert_code_mode_declaration_fields(
    description: &str,
    name: &str,
    input_name: &str,
    arg_fields: &[&str],
    output_fields: &[&str],
) {
    let declaration = parse_code_mode_declaration(description)
        .expect("description should include code-mode declaration");
    assert_eq!(declaration.name, name);
    assert_eq!(declaration.input_name, input_name);
    assert_eq!(declaration.args, normalize_code_mode_field_set(arg_fields));
    assert_eq!(
        declaration.output,
        normalize_code_mode_field_set(output_fields)
    );
}

fn compact_type(typ: &str) -> String {
    let mut compacted = String::with_capacity(typ.len());
    let mut quote_state = CodeModeQuoteState::default();

    for ch in typ.chars() {
        let was_escaped = quote_state.escaped;
        let was_in_quote = quote_state.in_quote();
        advance_code_mode_quote_state(ch, &mut quote_state);

        if was_escaped || was_in_quote || quote_state.in_quote() {
            compacted.push(ch);
            continue;
        }

        if !ch.is_whitespace() {
            compacted.push(ch);
        }
    }

    compacted
}

fn normalize_code_mode_field_set(fields: &[&str]) -> Vec<String> {
    let mut fields = fields
        .iter()
        .map(|field| compact_type(field))
        .collect::<Vec<_>>();
    fields.sort_unstable();
    fields
}

fn normalize_code_mode_type(ty: &str) -> Vec<String> {
    let ty = ty.trim();
    if ty.starts_with('{') && ty.ends_with('}') {
        normalize_code_mode_fields(&split_code_mode_fields(&ty[1..ty.len() - 1]))
    } else {
        vec![compact_type(ty)]
    }
}

fn normalize_code_mode_fields(fields: &[String]) -> Vec<String> {
    let mut fields = fields
        .iter()
        .map(|field| compact_type(field))
        .collect::<Vec<_>>();
    fields.sort_unstable();
    fields
}

fn empty_namespace_descriptions() -> BTreeMap<String, codex_code_mode::ToolNamespaceDescription> {
    BTreeMap::new()
}

fn update_plan_definition() -> codex_code_mode::ToolDefinition {
    codex_code_mode::ToolDefinition {
        name: "update_plan".to_string(),
        tool_name: ToolName::plain("update_plan"),
        all_tools_name: None,
        all_tools_module: None,
        description: "Update the plan".to_string(),
        kind: codex_code_mode::CodeModeToolKind::Function,
        input_schema: None,
        output_schema: None,
    }
}

fn split_code_mode_description(description: &str) -> Option<(&str, &str, &str)> {
    let (prose, after_wrapper) = description.split_once("\n\nexec tool declaration:\n```ts\n")?;
    let (declaration, trailing) = after_wrapper.split_once("\n```")?;
    Some((prose, declaration, trailing))
}

fn parse_code_mode_declaration(description: &str) -> Option<ParsedCodeModeDeclaration> {
    let declaration = split_code_mode_description(description)?.1.trim();
    let body = declaration.split_once("declare const tools:")?.1.trim();
    let body = body.strip_prefix("{")?.trim();
    let body = body.strip_suffix("};")?.trim();

    let open_paren = body.find('(')?;
    let (name, args_and_return) = body.split_at(open_paren);
    let args_and_return = &args_and_return[1..];
    let close_call = matching_paren_end(args_and_return)?;
    let (decl_input_name, args) = args_and_return[..close_call].split_once(':')?;
    let mut output_tail = args_and_return[close_call + 1..].trim_start();
    output_tail = output_tail.strip_prefix(":")?;
    let output_tail = output_tail.trim_start();
    let output_tail = output_tail.strip_prefix("Promise<")?;
    let output_end = matching_generic_end(output_tail)?;

    Some(ParsedCodeModeDeclaration {
        name: compact_type(name),
        input_name: compact_type(decl_input_name),
        args: normalize_code_mode_type(args),
        output: normalize_code_mode_type(&output_tail[..output_end]),
    })
}

fn matching_paren_end(typ: &str) -> Option<usize> {
    let mut depth = 1usize;
    let mut quote_state = CodeModeQuoteState::default();

    for (idx, ch) in typ.char_indices() {
        let was_escaped = quote_state.escaped;
        let was_in_quote = quote_state.in_quote();
        advance_code_mode_quote_state(ch, &mut quote_state);
        if was_escaped || was_in_quote {
            continue;
        }

        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }

    None
}

fn matching_generic_end(typ: &str) -> Option<usize> {
    let mut depth = 1usize;
    let mut quote_state = CodeModeQuoteState::default();

    for (idx, ch) in typ.char_indices() {
        let was_escaped = quote_state.escaped;
        let was_in_quote = quote_state.in_quote();
        advance_code_mode_quote_state(ch, &mut quote_state);
        if was_escaped || was_in_quote {
            continue;
        }

        match ch {
            '<' => depth += 1,
            '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }

    None
}

fn split_code_mode_fields(fields: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut brace_depth = 0usize;
    let mut angle_depth = 0usize;
    let mut square_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut quote_state = CodeModeQuoteState::default();

    for (idx, ch) in fields.char_indices() {
        let was_escaped = quote_state.escaped;
        let was_in_quote = quote_state.in_quote();
        advance_code_mode_quote_state(ch, &mut quote_state);
        if was_escaped || was_in_quote {
            continue;
        }

        match ch {
            '{' => brace_depth += 1,
            '}' if brace_depth > 0 => brace_depth -= 1,
            '<' => angle_depth += 1,
            '>' if angle_depth > 0 => angle_depth -= 1,
            '[' => square_depth += 1,
            ']' if square_depth > 0 => square_depth -= 1,
            '(' => paren_depth += 1,
            ')' if paren_depth > 0 => paren_depth -= 1,
            ';' if brace_depth == 0
                && angle_depth == 0
                && square_depth == 0
                && paren_depth == 0 =>
            {
                parts.push(fields[start..idx].trim().to_string());
                start = idx + 1;
            }
            _ => {}
        }
    }

    if start < fields.len() {
        let tail = fields[start..].trim();
        if !tail.is_empty() {
            parts.push(tail.to_string());
        }
    }

    parts
}

#[test]
fn augment_tool_spec_for_code_mode_augments_function_tools() {
    let spec = ToolSpec::Function(ResponsesApiTool {
        name: "lookup_order".to_string(),
        description: "Look up an order".to_string(),
        strict: false,
        defer_loading: Some(true),
        parameters: JsonSchema::object(
            BTreeMap::from([(
                "order_id".to_string(),
                JsonSchema::string(/*description*/ None),
            )]),
            Some(vec!["order_id".to_string()]),
            Some(AdditionalProperties::Boolean(false)),
        ),
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"}
            },
            "required": ["ok"],
        })),
    });
    let ToolSpec::Function(tool) = augment_tool_spec_for_code_mode(spec) else {
        panic!("tool mode should remain Function");
    };

    assert_eq!(tool.name, "lookup_order");
    assert_eq!(tool.strict, false);
    assert_eq!(tool.defer_loading, Some(true));
    assert_eq!(
        tool.parameters,
        JsonSchema::object(
            BTreeMap::from([(
                "order_id".to_string(),
                JsonSchema::string(/*description*/ None),
            )]),
            Some(vec!["order_id".to_string()]),
            Some(AdditionalProperties::Boolean(false)),
        )
    );
    assert_eq!(
        tool.output_schema,
        Some(json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"}
            },
            "required": ["ok"],
        }))
    );
    assert_code_mode_description(
        &tool.description,
        "Look up an order",
        "lookup_order",
        "args",
        &["order_id: string"],
        &["ok: boolean"],
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

    let definition = tool_spec_to_code_mode_tool_definition(&spec)
        .expect("tool should be converted to code-mode tool definition");
    assert_eq!(definition.name, "apply_patch");
    assert_eq!(definition.tool_name, ToolName::plain("apply_patch"));
    assert_eq!(definition.all_tools_name, None);
    assert_eq!(definition.all_tools_module, None);
    assert_eq!(definition.kind, codex_code_mode::CodeModeToolKind::Freeform);
    assert_eq!(definition.input_schema, None);
    assert_eq!(definition.output_schema, None);
    assert_code_mode_description(
        &definition.description,
        "Apply a patch",
        "apply_patch",
        "input",
        &["string"],
        &["unknown"],
    );
}

#[test]
fn tool_spec_to_code_mode_tool_definition_preserves_mcp_module_metadata() {
    let spec = ToolSpec::Function(ResponsesApiTool {
        name: "mcp__rmcp__echo".to_string(),
        description: "Echo text".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([(
                "message".to_string(),
                JsonSchema::string(/*description*/ None),
            )]),
            Some(vec!["message".to_string()]),
            Some(AdditionalProperties::Boolean(false)),
        ),
        output_schema: Some(json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"}
            },
            "required": ["ok"],
        })),
    });

    let definition = tool_spec_to_code_mode_tool_definition(&spec)
        .expect("tool should be converted to code-mode tool definition");
    assert_eq!(definition.name, "mcp__rmcp__echo");
    assert_eq!(definition.tool_name, ToolName::plain("mcp__rmcp__echo"));
    assert_eq!(definition.all_tools_name, Some("echo".to_string()));
    assert_eq!(
        definition.all_tools_module,
        Some("tools/mcp/rmcp.js".to_string())
    );
    assert_eq!(definition.kind, codex_code_mode::CodeModeToolKind::Function);
    assert_eq!(
        definition.input_schema,
        Some(json!({
            "type": "object",
            "properties": {
                "message": {"type": "string"}
            },
            "required": ["message"],
            "additionalProperties": false
        }))
    );
    assert_eq!(
        definition.output_schema,
        Some(json!({
            "type": "object",
            "properties": {
                "ok": {"type": "boolean"}
            },
            "required": ["ok"],
        }))
    );
    assert_code_mode_description(
        &definition.description,
        "Echo text",
        "mcp__rmcp__echo",
        "args",
        &["message: string"],
        &["ok: boolean"],
    );
}

#[test]
fn tool_spec_to_code_mode_tool_definition_skips_unsupported_variants() {
    assert_eq!(
        tool_spec_to_code_mode_tool_definition(&ToolSpec::ToolSearch {
            execution: "sync".to_string(),
            description: "Search".to_string(),
            parameters: JsonSchema::object(
                BTreeMap::new(),
                /*required*/ None,
                /*additional_properties*/ None,
            ),
        }),
        None
    );
}
