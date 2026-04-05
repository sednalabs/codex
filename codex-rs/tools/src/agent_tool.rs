use crate::JsonSchema;
use crate::ResponsesApiTool;
use crate::ToolSpec;
use codex_protocol::openai_models::ModelPreset;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub struct SpawnAgentToolOptions<'a> {
    pub available_models: &'a [ModelPreset],
    pub agent_type_description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitAgentTimeoutOptions {
    pub default_timeout_ms: i64,
    pub min_timeout_ms: i64,
    pub max_timeout_ms: i64,
}

pub fn create_spawn_agent_tool_v1(options: SpawnAgentToolOptions<'_>) -> ToolSpec {
    let available_models_description = spawn_agent_models_description(options.available_models);
    let return_value_description =
        "Returns the spawned agent id plus the user-facing nickname when available.";
    let properties = spawn_agent_common_properties(&options.agent_type_description);

    ToolSpec::Function(ResponsesApiTool {
        name: "spawn_agent".to_string(),
        description: spawn_agent_tool_description(
            &available_models_description,
            return_value_description,
        ),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties,
            required: None,
            additional_properties: Some(false.into()),
        },
        output_schema: Some(spawn_agent_output_schema_v1()),
    })
}

pub fn create_spawn_agent_tool_v2(options: SpawnAgentToolOptions<'_>) -> ToolSpec {
    let available_models_description = spawn_agent_models_description(options.available_models);
    let return_value_description = "Returns the canonical task name for the spawned agent, plus the user-facing nickname when available.";
    let mut properties = spawn_agent_common_properties(&options.agent_type_description);
    properties.insert(
        "task_name".to_string(),
        JsonSchema::String {
            description: Some(
                "Task name for the new agent. Use lowercase letters, digits, and underscores."
                    .to_string(),
            ),
        },
    );

    ToolSpec::Function(ResponsesApiTool {
        name: "spawn_agent".to_string(),
        description: spawn_agent_tool_description(
            &available_models_description,
            return_value_description,
        ),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["task_name".to_string()]),
            additional_properties: Some(false.into()),
        },
        output_schema: Some(spawn_agent_output_schema_v2()),
    })
}

pub fn create_send_input_tool_v1() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "target".to_string(),
            JsonSchema::String {
                description: Some("Agent id to message (from spawn_agent).".to_string()),
            },
        ),
        (
            "message".to_string(),
            JsonSchema::String {
                description: Some(
                    "Legacy plain-text message to send to the agent. Use either message or items."
                        .to_string(),
                ),
            },
        ),
        ("items".to_string(), create_collab_input_items_schema()),
        (
            "interrupt".to_string(),
            JsonSchema::Boolean {
                description: Some(
                    "When true, stop the agent's current task and handle this immediately. When false (default), queue this message."
                        .to_string(),
                ),
            },
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "send_input".to_string(),
        description: "Send a message to an existing agent. Use interrupt=true to redirect work immediately. You should reuse the agent by send_input if you believe your assigned task is highly dependent on the context of a previous task."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["target".to_string()]),
            additional_properties: Some(false.into()),
        },
        output_schema: Some(send_input_output_schema()),
    })
}

pub fn create_send_message_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "target".to_string(),
            JsonSchema::String {
                description: Some(
                    "Agent id or canonical task name to message (from spawn_agent).".to_string(),
                ),
            },
        ),
        ("items".to_string(), create_collab_input_items_schema()),
        (
            "interrupt".to_string(),
            JsonSchema::Boolean {
                description: Some(
                    "When true, stop the agent's current task and handle this immediately. When false (default), queue this message."
                        .to_string(),
                ),
            },
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "send_message".to_string(),
        description: "Add a message to an existing agent without triggering a new turn. Use interrupt=true to stop the current task first. In MultiAgentV2, this tool currently supports text content only."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["target".to_string(), "items".to_string()]),
            additional_properties: Some(false.into()),
        },
        output_schema: Some(send_input_output_schema()),
    })
}

pub fn create_assign_task_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "target".to_string(),
            JsonSchema::String {
                description: Some(
                    "Agent id or canonical task name to message (from spawn_agent).".to_string(),
                ),
            },
        ),
        ("items".to_string(), create_collab_input_items_schema()),
        (
            "interrupt".to_string(),
            JsonSchema::Boolean {
                description: Some(
                    "When true, stop the agent's current task and handle this immediately. When false (default), queue this message."
                        .to_string(),
                ),
            },
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "assign_task".to_string(),
        description: "Add a message to an existing agent and trigger a turn in the target. Use interrupt=true to redirect work immediately. In MultiAgentV2, this tool currently supports text content only."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["target".to_string(), "items".to_string()]),
            additional_properties: Some(false.into()),
        },
        output_schema: Some(send_input_output_schema()),
    })
}

pub fn create_resume_agent_tool() -> ToolSpec {
    let properties = BTreeMap::from([(
        "id".to_string(),
        JsonSchema::String {
            description: Some("Agent id to resume.".to_string()),
        },
    )]);

    ToolSpec::Function(ResponsesApiTool {
        name: "resume_agent".to_string(),
        description:
            "Resume a previously closed agent by id so it can receive send_input and wait_agent calls."
                .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["id".to_string()]),
            additional_properties: Some(false.into()),
        },
        output_schema: Some(resume_agent_output_schema()),
    })
}

pub fn create_wait_agent_tool_v1(options: WaitAgentTimeoutOptions) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: "wait_agent".to_string(),
        description: "Use this for blocking coordination while awaiting sub-agent completion. Returns a wait summary instead of the agent's final content, including the requested ids, any pending ids, and the completion reason. Prefer longer timeouts to avoid busy polling. When `return_when` is `any`, the call returns once one requested agent reaches terminal status. When `return_when` is `all`, it waits until every requested agent reaches terminal status."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: wait_agent_tool_parameters_v1(options),
        output_schema: Some(wait_output_schema_v1()),
    })
}

pub fn create_wait_agent_tool_v2(options: WaitAgentTimeoutOptions) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: "wait_agent".to_string(),
        description: "Use this for blocking coordination while awaiting sub-agent completion. Returns a brief wait summary instead of the agent's final content. Returns a timeout summary when no agent reaches a final status before the deadline. Prefer longer timeouts to avoid busy polling. When `return_when` is `any`, the call returns once one requested agent reaches terminal status. When `return_when` is `all`, it waits until every requested agent reaches terminal status."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: wait_agent_tool_parameters_v2(options),
        output_schema: Some(wait_output_schema_v2()),
    })
}

pub fn create_list_agents_tool() -> ToolSpec {
    let properties = BTreeMap::from([(
        "path_prefix".to_string(),
        JsonSchema::String {
            description: Some(
                "Optional task-path prefix. Accepts the same relative or absolute task-path syntax as other agent targets."
                    .to_string(),
            ),
        },
    )]);

    ToolSpec::Function(ResponsesApiTool {
        name: "list_agents".to_string(),
        description:
            "List live agents in the current root thread tree. Optionally filter by task-path prefix, and flag rows that still own active sub-agents."
                .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties,
            required: None,
            additional_properties: Some(false.into()),
        },
        output_schema: Some(list_agents_output_schema()),
    })
}

pub fn create_inspect_agent_tree_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "target".to_string(),
            JsonSchema::String {
                description: Some(
                    "Optional task-path root to inspect. Accepts the same relative or absolute task-path syntax as other agent targets. When omitted, inspects the current agent subtree."
                        .to_string(),
                ),
            },
        ),
        (
            "agent_roots".to_string(),
            JsonSchema::Array {
                items: Box::new(JsonSchema::String {
                    description: Some(
                        "Task-path roots to keep in the returned tree. Each value accepts the same relative or absolute task-path syntax as other agent targets; matching rows include the named agent and its descendants."
                            .to_string(),
                    ),
                }),
                description: Some(
                    "Optional branch filters for the inspected tree. When omitted, all matching descendants under the target are eligible."
                        .to_string(),
                ),
            },
        ),
        (
            "scope".to_string(),
            JsonSchema::String {
                description: Some(
                    "Optional inspection scope. Use `live` for active sessions only, `stale` for persisted closed descendants only, or `all` to combine both. Defaults to `live`."
                        .to_string(),
                ),
            },
        ),
        (
            "max_depth".to_string(),
            JsonSchema::Number {
                description: Some(
                    "Optional maximum descendant depth to return, counted from the inspected subtree root. Defaults to 2."
                        .to_string(),
                ),
            },
        ),
        (
            "max_agents".to_string(),
            JsonSchema::Number {
                description: Some(
                    "Optional maximum number of rows to return after tree ordering is applied. Defaults to 25."
                        .to_string(),
                ),
            },
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: "inspect_agent_tree".to_string(),
        description: "Inspect a compact nested agent tree for the current subtree or a target task path. Returns tree rows, live-or-stale session state, optional branch-filter context, and summary counts without dumping full transcripts."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties,
            required: None,
            additional_properties: Some(false.into()),
        },
        output_schema: Some(inspect_agent_tree_output_schema()),
    })
}

pub fn create_close_agent_tool_v1() -> ToolSpec {
    let properties = BTreeMap::from([(
        "target".to_string(),
        JsonSchema::String {
            description: Some("Agent id to close (from spawn_agent).".to_string()),
        },
    )]);

    ToolSpec::Function(ResponsesApiTool {
        name: "close_agent".to_string(),
        description: "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Don't keep agents open for too long if they are not needed anymore.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["target".to_string()]),
            additional_properties: Some(false.into()),
        },
        output_schema: Some(close_agent_output_schema()),
    })
}

pub fn create_close_agent_tool_v2() -> ToolSpec {
    let properties = BTreeMap::from([(
        "target".to_string(),
        JsonSchema::String {
            description: Some(
                "Agent id or canonical task name to close (from spawn_agent).".to_string(),
            ),
        },
    )]);

    ToolSpec::Function(ResponsesApiTool {
        name: "close_agent".to_string(),
        description: "Close an agent and any open descendants when they are no longer needed, and return the target agent's previous status before shutdown was requested. Don't keep agents open for too long if they are not needed anymore.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::Object {
            properties,
            required: Some(vec!["target".to_string()]),
            additional_properties: Some(false.into()),
        },
        output_schema: Some(close_agent_output_schema()),
    })
}

fn agent_status_output_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "string",
                "enum": ["pending_init", "running", "shutdown", "not_found"]
            },
            {
                "type": "object",
                "properties": {
                    "completed": {
                        "type": ["string", "null"]
                    }
                },
                "required": ["completed"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "errored": {
                        "type": "string"
                    }
                },
                "required": ["errored"],
                "additionalProperties": false
            }
        ]
    })
}

fn spawn_agent_output_schema_v1() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": {
                "type": "string",
                "description": "Thread identifier for the spawned agent."
            },
            "nickname": {
                "type": ["string", "null"],
                "description": "User-facing nickname for the spawned agent when available."
            },
            "role": {
                "type": ["string", "null"],
                "description": "Assigned role for the spawned agent when available."
            },
            "status": {
                "description": "Last known status of the spawned agent.",
                "allOf": [agent_status_output_schema()]
            },
            "requested_model": {
                "type": ["string", "null"],
                "description": "Model slug explicitly requested for the spawned agent when provided."
            },
            "requested_reasoning_effort": {
                "type": ["string", "null"],
                "enum": [null, "none", "minimal", "low", "medium", "high", "xhigh"],
                "description": "Reasoning effort explicitly requested for the spawned agent when provided."
            },
            "effective_model": {
                "type": ["string", "null"],
                "description": "Effective model resolved for the spawned agent when available."
            },
            "effective_reasoning_effort": {
                "type": ["string", "null"],
                "enum": [null, "none", "minimal", "low", "medium", "high", "xhigh"],
                "description": "Effective reasoning effort resolved for the spawned agent when available."
            },
            "effective_model_provider_id": {
                "type": "string",
                "description": "Model provider id resolved for the spawned agent."
            },
            "identity_source": {
                "type": "string",
                "description": "Source used to derive the agent identity metadata."
            }
        },
        "required": [
            "agent_id",
            "nickname",
            "role",
            "status",
            "requested_model",
            "requested_reasoning_effort",
            "effective_model",
            "effective_reasoning_effort",
            "effective_model_provider_id",
            "identity_source"
        ],
        "additionalProperties": false
    })
}

fn spawn_agent_output_schema_v2() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agent_id": {
                "type": ["string", "null"],
                "description": "Legacy thread identifier for the spawned agent."
            },
            "task_name": {
                "type": "string",
                "description": "Canonical task name for the spawned agent."
            },
            "nickname": {
                "type": ["string", "null"],
                "description": "User-facing nickname for the spawned agent when available."
            },
            "requested_model": {
                "type": ["string", "null"],
                "description": "Model slug explicitly requested for the spawned agent when provided."
            },
            "requested_reasoning_effort": {
                "type": ["string", "null"],
                "enum": [null, "none", "minimal", "low", "medium", "high", "xhigh"],
                "description": "Reasoning effort explicitly requested for the spawned agent when provided."
            },
            "effective_model": {
                "type": ["string", "null"],
                "description": "Effective model reported back for the spawned agent when available."
            },
            "effective_reasoning_effort": {
                "type": ["string", "null"],
                "enum": [null, "none", "minimal", "low", "medium", "high", "xhigh"],
                "description": "Effective reasoning effort reported back for the spawned agent when available."
            }
        },
        "required": [
            "agent_id",
            "task_name",
            "nickname",
            "requested_model",
            "requested_reasoning_effort",
            "effective_model",
            "effective_reasoning_effort"
        ],
        "additionalProperties": false
    })
}

fn send_input_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "submission_id": {
                "type": "string",
                "description": "Identifier for the queued input submission."
            }
        },
        "required": ["submission_id"],
        "additionalProperties": false
    })
}

fn list_agents_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "agents": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "agent_name": {
                            "type": "string",
                            "description": "Canonical task name for the agent when available, otherwise the agent id."
                        },
                        "agent_status": {
                            "description": "Last known status of the agent.",
                            "allOf": [agent_status_output_schema()]
                        },
                        "last_task_message": {
                            "type": ["string", "null"],
                            "description": "Most recent user or inter-agent instruction received by the agent, when available."
                        },
                        "has_active_subagents": {
                            "type": "boolean",
                            "description": "Whether the agent currently has any live descendants whose status is still non-final."
                        },
                        "active_subagent_count": {
                            "type": "integer",
                            "description": "Number of live descendants below this row whose status is still non-final."
                        }
                    },
                    "required": [
                        "agent_name",
                        "agent_status",
                        "last_task_message",
                        "has_active_subagents",
                        "active_subagent_count"
                    ],
                    "additionalProperties": false
                },
                "description": "Live agents visible in the current root thread tree."
            }
        },
        "required": ["agents"],
        "additionalProperties": false
    })
}

fn inspect_agent_tree_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "root_agent_name": {
                "type": "string",
                "description": "Canonical task path for the inspected subtree root when available, otherwise the agent id."
            },
            "scope_applied": {
                "type": "string",
                "enum": ["live", "stale", "all"],
                "description": "Inspection scope applied to the returned tree."
            },
            "agent_roots_applied": {
                "type": "array",
                "items": {
                    "type": "string"
                },
                "description": "Resolved task-path branch filters applied to the returned tree. Empty when no branch filter is active."
            },
            "max_depth_applied": {
                "type": "integer",
                "description": "Maximum descendant depth included in the returned rows."
            },
            "max_agents_applied": {
                "type": "integer",
                "description": "Maximum number of rows included in the returned tree."
            },
            "truncated": {
                "type": "boolean",
                "description": "Whether additional matching descendants were omitted because of the applied depth or row limits."
            },
            "summary": {
                "type": "object",
                "properties": {
                    "total_agents": { "type": "integer" },
                    "live_agents": { "type": "integer" },
                    "stale_agents": { "type": "integer" },
                    "pending_init_agents": { "type": "integer" },
                    "running_agents": { "type": "integer" },
                    "interrupted_agents": { "type": "integer" },
                    "completed_agents": { "type": "integer" },
                    "errored_agents": { "type": "integer" },
                    "shutdown_agents": { "type": "integer" },
                    "not_found_agents": { "type": "integer" }
                },
                "required": [
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
                ],
                "additionalProperties": false
            },
            "agents": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "agent_name": {
                            "type": "string",
                            "description": "Canonical task path for the row when available, otherwise the agent id."
                        },
                        "depth": {
                            "type": "integer",
                            "description": "Descendant depth relative to the inspected subtree root."
                        },
                        "session_state": {
                            "type": "string",
                            "enum": ["live", "stale"],
                            "description": "Whether the row comes from the live in-memory tree or a persisted closed descendant."
                        },
                        "agent_status": {
                            "type": ["object", "string", "null"],
                            "description": "Live agent status when the session is active; null for stale rows."
                        },
                        "nickname": {
                            "type": ["string", "null"],
                            "description": "User-facing nickname for the agent when available."
                        },
                        "role": {
                            "type": ["string", "null"],
                            "description": "Assigned role for the agent when available."
                        },
                        "direct_child_count": {
                            "type": "integer",
                            "description": "Number of direct descendants under this row within the selected scope."
                        },
                        "descendant_count": {
                            "type": "integer",
                            "description": "Total number of descendants below this row within the selected scope."
                        },
                        "last_task_message_preview": {
                            "type": ["string", "null"],
                            "description": "Compact preview of the latest known instruction for live rows, when available."
                        }
                    },
                    "required": [
                        "agent_name",
                        "depth",
                        "session_state",
                        "agent_status",
                        "nickname",
                        "role",
                        "direct_child_count",
                        "descendant_count",
                        "last_task_message_preview"
                    ],
                    "additionalProperties": false
                },
                "description": "Compact tree rows ordered depth-first from the inspected subtree root."
            }
        },
        "required": [
            "root_agent_name",
            "scope_applied",
            "agent_roots_applied",
            "max_depth_applied",
            "max_agents_applied",
            "truncated",
            "summary",
            "agents"
        ],
        "additionalProperties": false
    })
}

fn resume_agent_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": agent_status_output_schema()
        },
        "required": ["status"],
        "additionalProperties": false
    })
}

fn wait_output_schema_v1() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Brief wait summary without the agent's final content."
            },
            "requested_ids": {
                "type": "array",
                "items": {
                    "type": "string"
                },
                "description": "Resolved agent ids requested by the wait call."
            },
            "pending_ids": {
                "type": "array",
                "items": {
                    "type": "string"
                },
                "description": "Resolved agent ids still not terminal when the wait call completed."
            },
            "completion_reason": {
                "type": "string",
                "enum": ["terminal", "timeout"],
                "description": "Why the wait call completed."
            },
            "timed_out": {
                "type": "boolean",
                "description": "Whether the wait call returned due to timeout before the requested return condition was satisfied."
            }
        },
        "required": [
            "message",
            "requested_ids",
            "pending_ids",
            "completion_reason",
            "timed_out"
        ],
        "additionalProperties": false
    })
}

fn wait_output_schema_v2() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": {
                "type": "string",
                "description": "Brief wait summary without the agent's final content."
            },
            "timed_out": {
                "type": "boolean",
                "description": "Whether the wait call returned due to timeout before the requested return condition was satisfied."
            }
        },
        "required": ["message", "timed_out"],
        "additionalProperties": false
    })
}

fn close_agent_output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "previous_status": {
                "description": "The agent status observed before shutdown was requested.",
                "allOf": [agent_status_output_schema()]
            }
        },
        "required": ["previous_status"],
        "additionalProperties": false
    })
}

fn create_collab_input_items_schema() -> JsonSchema {
    let properties = BTreeMap::from([
        (
            "type".to_string(),
            JsonSchema::String {
                description: Some(
                    "Input item type: text, image, local_image, skill, or mention.".to_string(),
                ),
            },
        ),
        (
            "text".to_string(),
            JsonSchema::String {
                description: Some("Text content when type is text.".to_string()),
            },
        ),
        (
            "image_url".to_string(),
            JsonSchema::String {
                description: Some("Image URL when type is image.".to_string()),
            },
        ),
        (
            "path".to_string(),
            JsonSchema::String {
                description: Some(
                    "Path when type is local_image/skill, or structured mention target such as app://<connector-id> or plugin://<plugin-name>@<marketplace-name> when type is mention."
                        .to_string(),
                ),
            },
        ),
        (
            "name".to_string(),
            JsonSchema::String {
                description: Some("Display name when type is skill or mention.".to_string()),
            },
        ),
    ]);

    JsonSchema::Array {
        items: Box::new(JsonSchema::Object {
            properties,
            required: None,
            additional_properties: Some(false.into()),
        }),
        description: Some(
            "Structured input items. Use this to pass explicit mentions (for example app:// connector paths)."
                .to_string(),
        ),
    }
}

fn spawn_agent_common_properties(agent_type_description: &str) -> BTreeMap<String, JsonSchema> {
    BTreeMap::from([
        (
            "message".to_string(),
            JsonSchema::String {
                description: Some(
                    "Initial plain-text task for the new agent. Use either message or items."
                        .to_string(),
                ),
            },
        ),
        ("items".to_string(), create_collab_input_items_schema()),
        (
            "agent_type".to_string(),
            JsonSchema::String {
                description: Some(agent_type_description.to_string()),
            },
        ),
        (
            "fork_context".to_string(),
            JsonSchema::Boolean {
                description: Some(
                    "When true, fork the current thread history into the new agent before sending the initial prompt. This must be used when you want the new agent to have exactly the same context as you."
                        .to_string(),
                ),
            },
        ),
        (
            "model".to_string(),
            JsonSchema::String {
                description: Some(
                    "Optional model override for the new agent. Replaces the inherited model."
                        .to_string(),
                ),
            },
        ),
        (
            "reasoning_effort".to_string(),
            JsonSchema::String {
                description: Some(
                    "Optional reasoning effort override for the new agent. Replaces the inherited reasoning effort."
                        .to_string(),
                ),
            },
        ),
    ])
}

fn spawn_agent_tool_description(
    available_models_description: &str,
    return_value_description: &str,
) -> String {
    format!(
        r#"
        Only use `spawn_agent` if and only if the user explicitly asks for sub-agents, delegation, or parallel agent work.
        Requests for depth, thoroughness, research, investigation, or detailed codebase analysis do not count as permission to spawn.
        Agent-role guidance below only helps choose which agent to use after spawning is already authorized; it never authorizes spawning by itself.
        Spawn a sub-agent for a well-scoped task. {return_value_description} This spawn_agent tool provides you access to smaller but more efficient sub-agents. A mini model can solve many tasks faster than the main model. You should follow the rules and guidelines below to use this tool.

{available_models_description}
### When to delegate vs. do the subtask yourself
- First, quickly analyze the overall user task and form a succinct high-level plan. Identify which tasks are immediate blockers on the critical path, and which tasks are sidecar tasks that are needed but can run in parallel without blocking the next local step. As part of that plan, explicitly decide what immediate task you should do locally right now. Do this planning step before delegating to agents so you do not hand off the immediate blocking task to a submodel and then waste time waiting on it.
- Use the smaller subagent when a subtask is easy enough for it to handle and can run in parallel with your local work. Prefer delegating concrete, bounded sidecar tasks that materially advance the main task without blocking your immediate next local step.
- Do not delegate urgent blocking work when your immediate next step depends on that result. If the very next action is blocked on that task, the main rollout should usually do it locally to keep the critical path moving.
- Keep work local when the subtask is too difficult to delegate well and when it is tightly coupled, urgent, or likely to block your immediate next step.

### Designing delegated subtasks
- Subtasks must be concrete, well-defined, and self-contained.
- Delegated subtasks must materially advance the main task.
- Do not duplicate work between the main rollout and delegated subtasks.
- Avoid issuing multiple delegate calls on the same unresolved thread unless the new delegated task is genuinely different and necessary.
- Narrow the delegated ask to the concrete output you need next.
- For coding tasks, prefer delegating concrete code-change worker subtasks over read-only explorer analysis when the subagent can make a bounded patch in a clear write scope.
- When delegating coding work, instruct the submodel to edit files directly in its forked workspace and list the file paths it changed in the final answer.
- For code-edit subtasks, decompose work so each delegated task has a disjoint write set.

### Model selection waterfall
- For same-workspace analysis or implementation, prefer a native Codex sub-agent before any external fallback.
- Start with the smallest capable lane visible in the loaded model catalog above.
- When the loaded catalog includes it, use `gpt-5.1-codex-mini` first for bookkeeping, waiting, compact scouting, and other routine sidecar work.
- When the loaded catalog includes it, prefer `gpt-5.3-codex-spark` for read-heavy, output-light, file-local scouting or tiny edits when the subtask is unlikely to need a second substantial reasoning pass.
- When the loaded catalog includes it, escalate to `gpt-5.4-mini` when the subtask is still straightforward but needs richer context, tighter review, or a few related files.
- If those exact slugs are not loaded, keep the same cheap-first intent and pick the closest visible native Codex model instead of naming an unavailable model.
- Escalate beyond those defaults only when you can name the concrete reason the cheaper lane is insufficient.

### After you delegate
- Call wait_agent very sparingly. Only call wait_agent when you need the result immediately for the next critical-path step and you are blocked until it returns.
- For helper-backed waits (for example, `exec_command`/`write_stdin`), prefer `wait_until_terminal=true` so the tool layer blocks on a terminal state instead of transcript polling loops.
- Prefer list_agents for cheap live status snapshots before calling a blocking wait_agent, and use inspect_agent_tree when you need deeper live vs stale descendant state.
- Do not redo delegated subagent tasks yourself; focus on integrating results or tackling non-overlapping work.
- While the subagent is running in the background, do meaningful non-overlapping work immediately.
- Do not repeatedly wait by reflex.
- When a delegated coding task returns, quickly review the uploaded changes, then integrate or refine them.

### Parallel delegation patterns
- Run multiple independent information-seeking subtasks in parallel when you have distinct questions that can be answered independently.
- Split implementation into disjoint codebase slices and spawn multiple agents for them in parallel when the write scopes do not overlap.
- Delegate verification only when it can run in parallel with ongoing implementation and is likely to catch a concrete risk before final integration.
- The key is to find opportunities to spawn multiple independent subtasks in parallel within the same round, while ensuring each subtask is well-defined, self-contained, and materially advances the main task."#
    )
}

fn spawn_agent_models_description(models: &[ModelPreset]) -> String {
    let visible_models: Vec<&ModelPreset> =
        models.iter().filter(|model| model.show_in_picker).collect();
    if visible_models.is_empty() {
        return "No picker-visible models are currently loaded.".to_string();
    }

    visible_models
        .into_iter()
        .map(|model| {
            let efforts = model
                .supported_reasoning_efforts
                .iter()
                .map(|preset| format!("{} ({})", preset.effort, preset.description))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "- {} (`{}`): {} Default reasoning effort: {}. Supported reasoning efforts: {}.",
                model.display_name,
                model.model,
                model.description,
                model.default_reasoning_effort,
                efforts
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_agent_tool_parameters_v1(options: WaitAgentTimeoutOptions) -> JsonSchema {
    let properties = BTreeMap::from([
        (
            "targets".to_string(),
            JsonSchema::Array {
                items: Box::new(JsonSchema::String { description: None }),
                description: Some(
                    "Agent ids to wait on. Pass multiple ids to coordinate several agents."
                        .to_string(),
                ),
            },
        ),
        (
            "ids".to_string(),
            JsonSchema::Array {
                items: Box::new(JsonSchema::String { description: None }),
                description: Some(
                    "Legacy alias for targets. Agent ids to wait on. Pass multiple ids to coordinate several agents."
                        .to_string(),
                ),
            },
        ),
        (
            "timeout_ms".to_string(),
            JsonSchema::Number {
                description: Some(format!(
                    "Optional timeout in milliseconds. Defaults to {}, min {}, max {}. Prefer longer waits (minutes) to avoid busy polling.",
                    options.default_timeout_ms, options.min_timeout_ms, options.max_timeout_ms,
                )),
            },
        ),
        (
            "return_when".to_string(),
            JsonSchema::String {
                description: Some(
                    "Wait mode. Use `any` to return when one requested agent reaches terminal status, or `all` to wait until every requested agent reaches terminal status."
                        .to_string(),
                ),
            },
        ),
    ]);

    JsonSchema::Object {
        properties,
        required: None,
        additional_properties: Some(false.into()),
    }
}

fn wait_agent_tool_parameters_v2(options: WaitAgentTimeoutOptions) -> JsonSchema {
    let properties = BTreeMap::from([
        (
            "targets".to_string(),
            JsonSchema::Array {
                items: Box::new(JsonSchema::String { description: None }),
                description: Some(
                    "Agent ids or canonical task names to wait on. Pass multiple targets to coordinate several agents."
                        .to_string(),
                ),
            },
        ),
        (
            "timeout_ms".to_string(),
            JsonSchema::Number {
                description: Some(format!(
                    "Optional timeout in milliseconds. Defaults to {}, min {}, max {}. Prefer longer waits (minutes) to avoid busy polling.",
                    options.default_timeout_ms, options.min_timeout_ms, options.max_timeout_ms,
                )),
            },
        ),
        (
            "return_when".to_string(),
            JsonSchema::String {
                description: Some(
                    "Wait mode. Use `any` to return when one requested agent reaches terminal status, or `all` to wait until every requested agent reaches terminal status."
                        .to_string(),
                ),
            },
        ),
    ]);

    JsonSchema::Object {
        properties,
        required: Some(vec!["targets".to_string()]),
        additional_properties: Some(false.into()),
    }
}

#[cfg(test)]
#[path = "agent_tool_tests.rs"]
mod tests;
