use super::*;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use pretty_assertions::assert_eq;
use serde_json::json;

fn model_preset(id: &str, show_in_picker: bool) -> ModelPreset {
    ModelPreset {
        id: id.to_string(),
        model: format!("{id}-model"),
        display_name: format!("{id} display"),
        description: format!("{id} description"),
        default_reasoning_effort: ReasoningEffort::Medium,
        supported_reasoning_efforts: vec![ReasoningEffortPreset {
            effort: ReasoningEffort::Medium,
            description: "Balanced".to_string(),
        }],
        supports_personality: false,
        is_default: false,
        upgrade: None,
        show_in_picker,
        availability_nux: None,
        supported_in_api: true,
        input_modalities: Vec::new(),
    }
}

#[test]
fn spawn_agent_tool_v2_requires_task_name_and_lists_visible_models() {
    let tool = create_spawn_agent_tool_v2(SpawnAgentToolOptions {
        available_models: &[
            model_preset("visible", /*show_in_picker*/ true),
            model_preset("hidden", /*show_in_picker*/ false),
        ],
        agent_type_description: "role help".to_string(),
    });

    let ToolSpec::Function(ResponsesApiTool {
        description,
        parameters,
        output_schema,
        ..
    }) = tool
    else {
        panic!("spawn_agent should be a function tool");
    };
    let JsonSchema::Object {
        properties,
        required,
        ..
    } = parameters
    else {
        panic!("spawn_agent should use object params");
    };
    assert!(description.contains("visible display (`visible-model`)"));
    assert!(!description.contains("hidden display (`hidden-model`)"));
    assert!(properties.contains_key("task_name"));
    assert_eq!(
        properties.get("agent_type"),
        Some(&JsonSchema::String {
            description: Some("role help".to_string()),
        })
    );
    assert_eq!(required, Some(vec!["task_name".to_string()]));
    assert_eq!(
        output_schema.expect("spawn_agent output schema")["required"],
        json!(["agent_id", "task_name", "nickname"])
    );
}

#[test]
fn spawn_agent_tool_v1_exposes_runtime_metadata_fields() {
    let ToolSpec::Function(ResponsesApiTool { output_schema, .. }) =
        create_spawn_agent_tool_v1(SpawnAgentToolOptions {
            available_models: &[model_preset("visible", /*show_in_picker*/ true)],
            agent_type_description: "role help".to_string(),
        })
    else {
        panic!("spawn_agent should be a function tool");
    };
    assert_eq!(
        output_schema.expect("spawn_agent output schema")["required"],
        json!([
            "agent_id",
            "nickname",
            "role",
            "status",
            "effective_model",
            "effective_reasoning_effort",
            "effective_model_provider_id",
            "identity_source"
        ])
    );
}

#[test]
fn send_message_tool_requires_items_and_uses_submission_output() {
    let ToolSpec::Function(ResponsesApiTool {
        parameters,
        output_schema,
        ..
    }) = create_send_message_tool()
    else {
        panic!("send_message should be a function tool");
    };
    let JsonSchema::Object {
        properties,
        required,
        ..
    } = parameters
    else {
        panic!("send_message should use object params");
    };
    assert!(properties.contains_key("target"));
    assert!(properties.contains_key("items"));
    assert!(!properties.contains_key("message"));
    assert_eq!(
        required,
        Some(vec!["target".to_string(), "items".to_string()])
    );
    assert_eq!(
        output_schema.expect("send_message output schema")["required"],
        json!(["submission_id"])
    );
}

#[test]
fn wait_agent_tool_v1_exposes_return_when_and_summary_output() {
    let ToolSpec::Function(ResponsesApiTool {
        parameters,
        output_schema,
        ..
    }) = create_wait_agent_tool_v1(WaitAgentTimeoutOptions {
        default_timeout_ms: 30_000,
        min_timeout_ms: 10_000,
        max_timeout_ms: 3_600_000,
    })
    else {
        panic!("wait_agent should be a function tool");
    };
    let JsonSchema::Object { properties, .. } = parameters else {
        panic!("wait_agent should use object params");
    };
    assert!(properties.contains_key("return_when"));
    assert_eq!(
        output_schema.expect("wait output schema")["required"],
        json!([
            "message",
            "requested_ids",
            "pending_ids",
            "completion_reason",
            "timed_out"
        ])
    );
}

#[test]
fn wait_agent_tool_v2_uses_task_targets_and_summary_output() {
    let ToolSpec::Function(ResponsesApiTool {
        parameters,
        output_schema,
        ..
    }) = create_wait_agent_tool_v2(WaitAgentTimeoutOptions {
        default_timeout_ms: 30_000,
        min_timeout_ms: 10_000,
        max_timeout_ms: 3_600_000,
    })
    else {
        panic!("wait_agent should be a function tool");
    };
    let JsonSchema::Object { properties, .. } = parameters else {
        panic!("wait_agent should use object params");
    };
    assert!(properties.contains_key("return_when"));
    let Some(JsonSchema::Array {
        description: Some(description),
        ..
    }) = properties.get("targets")
    else {
        panic!("wait_agent should define targets array");
    };
    assert!(description.contains("canonical task names"));
    assert_eq!(
        output_schema.expect("wait output schema")["properties"]["message"]["description"],
        json!("Brief wait summary without the agent's final content.")
    );
}

#[test]
fn list_agents_tool_includes_path_prefix_and_agent_fields() {
    let ToolSpec::Function(ResponsesApiTool {
        parameters,
        output_schema,
        ..
    }) = create_list_agents_tool()
    else {
        panic!("list_agents should be a function tool");
    };
    let JsonSchema::Object { properties, .. } = parameters else {
        panic!("list_agents should use object params");
    };
    assert!(properties.contains_key("path_prefix"));
    assert_eq!(
        output_schema.expect("list_agents output schema")["properties"]["agents"]["items"]["required"],
        json!([
            "agent_name",
            "agent_status",
            "last_task_message",
            "has_active_subagents",
            "active_subagent_count"
        ])
    );
}

#[test]
fn inspect_agent_tree_tool_exposes_scope_and_compact_tree_fields() {
    let ToolSpec::Function(ResponsesApiTool {
        parameters,
        output_schema,
        ..
    }) = create_inspect_agent_tree_tool()
    else {
        panic!("inspect_agent_tree should be a function tool");
    };
    let JsonSchema::Object { properties, .. } = parameters else {
        panic!("inspect_agent_tree should use object params");
    };
    assert!(properties.contains_key("target"));
    assert!(properties.contains_key("agent_roots"));
    assert!(properties.contains_key("scope"));
    assert!(properties.contains_key("max_depth"));
    assert!(properties.contains_key("max_agents"));
    let output_schema = output_schema.expect("inspect_agent_tree output schema");
    assert_eq!(
        output_schema["required"],
        json!([
            "root_agent_name",
            "scope_applied",
            "agent_roots_applied",
            "max_depth_applied",
            "max_agents_applied",
            "truncated",
            "summary",
            "agents"
        ])
    );
    assert_eq!(
        output_schema["properties"]["summary"]["required"],
        json!([
            "total_agents",
            "live_agents",
            "stale_agents",
            "pending_init_agents",
            "running_agents",
            "interrupted_agents",
            "completed_agents",
            "errored_agents",
            "shutdown_agents",
            "not_found_agents"
        ])
    );
    assert_eq!(
        output_schema["properties"]["agents"]["items"]["required"],
        json!([
            "agent_name",
            "depth",
            "session_state",
            "agent_status",
            "nickname",
            "role",
            "direct_child_count",
            "descendant_count",
            "last_task_message_preview"
        ])
    );
}
