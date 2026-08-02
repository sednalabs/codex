use super::*;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ResponseItem;
use codex_tools::ToolExecutionStatus;
use codex_tools::ToolOutput;
use pretty_assertions::assert_eq;
use rmcp::model::AnnotateAble;
use rmcp::model::ResourceContents;
use serde_json::json;

use crate::context_manager::ContextManager;
use crate::tools::context::ToolCallSource;
use crate::tools::context::ToolPayload;

fn resource(uri: &str, name: &str) -> Resource {
    rmcp::model::RawResource {
        uri: uri.to_string(),
        name: name.to_string(),
        title: None,
        description: None,
        mime_type: None,
        size: None,
        icons: None,
        meta: None,
    }
    .no_annotation()
}

fn template(uri_template: &str, name: &str) -> ResourceTemplate {
    rmcp::model::RawResourceTemplate {
        uri_template: uri_template.to_string(),
        name: name.to_string(),
        title: None,
        description: None,
        mime_type: None,
        icons: None,
    }
    .no_annotation()
}

#[test]
fn resource_with_server_serializes_server_field() {
    let entry = ResourceWithServer::new("test".to_string(), resource("memo://id", "memo"));
    let value = serde_json::to_value(&entry).expect("serialize resource");

    assert_eq!(value["server"], json!("test"));
    assert_eq!(value["uri"], json!("memo://id"));
    assert_eq!(value["name"], json!("memo"));
}

#[test]
fn list_resources_payload_from_single_server_copies_next_cursor() {
    let result = ListResourcesResult {
        meta: None,
        next_cursor: Some("cursor-1".to_string()),
        resources: vec![resource("memo://id", "memo")],
    };
    let payload = ListResourcesPayload::from_single_server("srv".to_string(), result);
    let value = serde_json::to_value(&payload).expect("serialize payload");

    assert_eq!(value["server"], json!("srv"));
    assert_eq!(value["nextCursor"], json!("cursor-1"));
    let resources = value["resources"].as_array().expect("resources array");
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0]["server"], json!("srv"));
}

#[test]
fn list_resources_payload_from_all_servers_is_sorted() {
    let mut map = HashMap::new();
    map.insert("beta".to_string(), vec![resource("memo://b-1", "b-1")]);
    map.insert(
        "alpha".to_string(),
        vec![resource("memo://a-1", "a-1"), resource("memo://a-2", "a-2")],
    );

    let payload = ListResourcesPayload::from_all_servers(map);
    let value = serde_json::to_value(&payload).expect("serialize payload");
    let uris: Vec<String> = value["resources"]
        .as_array()
        .expect("resources array")
        .iter()
        .map(|entry| entry["uri"].as_str().unwrap().to_string())
        .collect();

    assert_eq!(
        uris,
        vec![
            "memo://a-1".to_string(),
            "memo://a-2".to_string(),
            "memo://b-1".to_string()
        ]
    );
}

#[test]
fn call_tool_result_from_content_marks_success() {
    let result = call_tool_result_from_content("{}", Some(true));
    assert_eq!(result.is_error, Some(false));
    assert_eq!(result.content.len(), 1);
}

#[test]
fn oversized_json_resource_keeps_code_mode_execution_successful() {
    let output = serialize_read_resource_output(
        text_resource_payload(
            "ops",
            "ops://resource/large-json",
            Some("application/json"),
            json!({ "items": ["x".repeat(255 * 1024)] }).to_string(),
        ),
        TruncationPolicy::Bytes(8_000),
    )
    .expect("serialize resource output");

    assert_eq!(output.model_success(), Some(false));
    assert_eq!(output.success_for_logging(), false);
    assert_eq!(
        output.execution_status_for_source(&ToolCallSource::Direct),
        ToolExecutionStatus::Failed
    );
    assert_eq!(
        output.execution_status_for_source(&ToolCallSource::CodeMode {
            cell_id: "cell-1".to_string(),
            runtime_tool_call_id: "tool-1".to_string(),
        }),
        ToolExecutionStatus::Completed
    );
    assert_eq!(
        output.code_mode_execution_status(),
        ToolExecutionStatus::Completed
    );
}

#[test]
fn parse_arguments_handles_empty_and_json() {
    assert!(
        parse_arguments(" \n\t").unwrap().is_none(),
        "expected None for empty arguments"
    );

    assert!(
        parse_arguments("null").unwrap().is_none(),
        "expected None for null arguments"
    );

    let value = parse_arguments(r#"{"server":"figma"}"#)
        .expect("parse json")
        .expect("value present");
    assert_eq!(value["server"], json!("figma"));
}

#[test]
fn template_with_server_serializes_server_field() {
    let entry = ResourceTemplateWithServer::new("srv".to_string(), template("memo://{id}", "memo"));
    let value = serde_json::to_value(&entry).expect("serialize template");

    assert_eq!(
        value,
        json!({
            "server": "srv",
            "uriTemplate": "memo://{id}",
            "name": "memo"
        })
    );
}

#[test]
fn serialize_function_output_preserves_small_payload() {
    let payload = json!({"server": "hosted", "resources": []});
    let expected = serde_json::to_string(&payload).expect("serialize payload");

    let output = serialize_function_output(payload, TruncationPolicy::Bytes(1_024))
        .expect("serialize function output")
        .into_text();

    assert_eq!(output, expected);
}

#[test]
fn serialize_function_output_caps_read_resource_payload() {
    let truncation_policy = TruncationPolicy::Bytes(8_000);
    let payload = ReadResourcePayload {
        server: "hosted".to_string(),
        uri: "skill://large/SKILL.md".to_string(),
        result: ReadResourceResult::new(vec![ResourceContents::TextResourceContents {
            uri: "skill://large/SKILL.md".to_string(),
            mime_type: Some("text/markdown".to_string()),
            text: "x".repeat(16_000),
            meta: None,
        }]),
    };
    let serialized = serde_json::to_string(&payload).expect("serialize payload");
    let expected = truncate_text(&serialized, truncation_policy * 1.2);

    let output = serialize_function_output(payload, truncation_policy)
        .expect("serialize bounded function output")
        .into_text();

    assert_ne!(output, serialized);
    assert_eq!(output, expected);
}

fn text_resource_payload(
    server: &str,
    uri: &str,
    mime_type: Option<&str>,
    text: String,
) -> ReadResourcePayload {
    ReadResourcePayload {
        server: server.to_string(),
        uri: uri.to_string(),
        result: ReadResourceResult::new(vec![ResourceContents::TextResourceContents {
            uri: uri.to_string(),
            mime_type: mime_type.map(str::to_string),
            text,
            meta: None,
        }]),
    }
}

fn json_resource_payload(text: String) -> ReadResourcePayload {
    text_resource_payload(
        "hosted",
        "ops://work_item/w10190/tree",
        Some("application/json; charset=utf-8"),
        text,
    )
}

#[test]
fn serialize_read_resource_output_preserves_small_json_for_model() {
    let resource_json = json!({"items": ["one", "two"]}).to_string();
    let output = serialize_read_resource_output(
        json_resource_payload(resource_json.clone()),
        TruncationPolicy::Bytes(8_000),
    )
    .expect("serialize resource output");

    assert_eq!(output.model_success(), Some(true));
    let model_payload: serde_json::Value =
        serde_json::from_str(&output.model_content()).expect("parse model envelope");
    let model_resource = model_payload["contents"][0]["text"]
        .as_str()
        .expect("resource text");
    let parsed_resource: serde_json::Value =
        serde_json::from_str(model_resource).expect("parse model resource JSON");
    assert_eq!(
        parsed_resource,
        serde_json::from_str::<serde_json::Value>(&resource_json).unwrap()
    );
}

#[test]
fn large_json_resource_fails_closed_for_model_and_preserves_code_mode_payload() {
    let resource_json = json!({"items": ["x".repeat(255 * 1024)]}).to_string();
    let output = serialize_read_resource_output(
        json_resource_payload(resource_json.clone()),
        TruncationPolicy::Bytes(8_000),
    )
    .expect("serialize resource output");

    assert_eq!(output.model_success(), Some(false));
    let model_content = output.model_content();
    assert!(!model_content.contains("tokens truncated"));
    let model_error: serde_json::Value =
        serde_json::from_str(&model_content).expect("parse bounded model error");
    assert_eq!(
        model_error,
        json!({
            "error": {
                "code": "mcp_resource_model_output_too_large",
                "message": "The resource contains JSON that exceeds the model output limit.",
                "truncated": true
            }
        })
    );
    assert!(model_error.get("contents").is_none());

    let raw_payload = output.code_mode_result(&ToolPayload::Function {
        arguments: "{}".to_string(),
    });
    let raw_content = raw_payload.as_str().expect("raw code-mode string");
    let raw_envelope: serde_json::Value =
        serde_json::from_str(raw_content).expect("parse raw code-mode envelope");
    let raw_resource = raw_envelope["contents"][0]["text"]
        .as_str()
        .expect("raw resource text");
    assert_eq!(raw_resource, resource_json);
    let _: serde_json::Value = serde_json::from_str(raw_resource).expect("parse raw JSON resource");
}

#[test]
fn serialize_read_resource_output_fails_closed_for_vendor_json_media_type() {
    let output = serialize_read_resource_output(
        text_resource_payload(
            "hosted",
            "ops://work_item/w10190/tree",
            Some("Application/Vnd.Sedna+Json; Charset=UTF-8"),
            json!({"items": ["x".repeat(255 * 1024)]}).to_string(),
        ),
        TruncationPolicy::Bytes(8_000),
    )
    .expect("serialize resource output");

    assert_eq!(output.model_success(), Some(false));
    let _: serde_json::Value =
        serde_json::from_str(&output.model_content()).expect("parse bounded model error");
}

#[test]
fn serialize_read_resource_output_fails_closed_for_problem_json_media_type() {
    let output = serialize_read_resource_output(
        text_resource_payload(
            "hosted",
            "ops://work_item/w10190/tree",
            Some("APPLICATION/PROBLEM+JSON; charset=utf-8"),
            json!({"items": ["x".repeat(255 * 1024)]}).to_string(),
        ),
        TruncationPolicy::Bytes(8_000),
    )
    .expect("serialize resource output");

    assert_eq!(output.model_success(), Some(false));
    let _: serde_json::Value =
        serde_json::from_str(&output.model_content()).expect("parse bounded model error");
}

#[test]
fn serialize_read_resource_output_keeps_untyped_json_on_generic_truncation() {
    let truncation_policy = TruncationPolicy::Bytes(8_000);
    let payload = text_resource_payload(
        "hosted",
        "ops://work_item/w10190/tree",
        /*mime_type*/ None,
        json!({"items": ["x".repeat(255 * 1024)]}).to_string(),
    );
    let expected = truncate_text(
        &serde_json::to_string(&payload).expect("serialize payload"),
        truncation_policy * 1.2,
    );
    let output = serialize_read_resource_output(payload, truncation_policy)
        .expect("serialize resource output");

    assert_eq!(output.model_success(), Some(true));
    assert_eq!(output.model_content(), expected);
}

#[test]
fn serialize_read_resource_output_keeps_large_markdown_on_generic_truncation() {
    let truncation_policy = TruncationPolicy::Bytes(8_000);
    let payload = text_resource_payload(
        "hosted",
        "skill://large/SKILL.md",
        Some("text/markdown"),
        "x".repeat(16_000),
    );
    let expected = truncate_text(
        &serde_json::to_string(&payload).expect("serialize payload"),
        truncation_policy * 1.2,
    );
    let output = serialize_read_resource_output(payload, truncation_policy)
        .expect("serialize resource output");

    assert_eq!(output.model_success(), Some(true));
    assert_eq!(output.model_content(), expected);
}

#[test]
fn history_does_not_retruncate_bounded_json_resource_error() {
    let output = serialize_read_resource_output(
        text_resource_payload(
            &"server-".repeat(64 * 1024),
            &format!("ops://{}", "resource-".repeat(64 * 1024)),
            Some("application/json"),
            json!({"items": ["x".repeat(255 * 1024)]}).to_string(),
        ),
        TruncationPolicy::Bytes(8_000),
    )
    .expect("serialize resource output");
    let expected_model_content = output.model_content();
    assert!(expected_model_content.len() < 1_024);
    assert!(!expected_model_content.contains("server-server"));
    assert!(!expected_model_content.contains("ops://resource"));
    let response_item = ResponseItem::from(output.to_response_item(
        "call-1",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    ));
    let function_call = ResponseItem::FunctionCall {
        id: None,
        name: "read_mcp_resource".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "call-1".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };
    let mut history = ContextManager::new();
    history.record_items(
        [&function_call, &response_item],
        TruncationPolicy::Bytes(9_600),
    );
    let prompt = history.for_prompt(&[]);
    let [
        ResponseItem::FunctionCall { .. },
        ResponseItem::FunctionCallOutput {
            output: recorded, ..
        },
    ] = prompt.as_slice()
    else {
        panic!("expected function call and output in model prompt");
    };

    assert_eq!(recorded.success, Some(false));
    let FunctionCallOutputBody::Text(recorded_text) = &recorded.body else {
        panic!("expected text output");
    };
    assert_eq!(recorded_text, &expected_model_content);
    let _: serde_json::Value = serde_json::from_str(recorded_text).expect("parse history error");
}
