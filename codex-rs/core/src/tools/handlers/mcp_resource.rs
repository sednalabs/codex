<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
=======
use std::collections::HashMap;
use std::future::Future;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_mcp::CODEX_APPS_MCP_SERVER_NAME;
use codex_mcp::McpResourceListing;
use codex_mcp::McpResourceListingFailure;
use codex_protocol::items::McpToolCallError;
use codex_protocol::items::McpToolCallItem;
use codex_protocol::items::McpToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::mcp::CallToolResult;
use codex_protocol::models::function_call_output_content_items_to_text;
use codex_protocol::protocol::TruncationPolicy;
use codex_utils_output_truncation::truncate_text;
use codex_utils_output_truncation::with_serialization_allowance;
use rmcp::model::ListResourceTemplatesResult;
use rmcp::model::ListResourcesResult;
use rmcp::model::PaginatedRequestParams;
use rmcp::model::ReadResourceResult;
use rmcp::model::Resource;
use rmcp::model::ResourceTemplate;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
use crate::tools::context::ToolPayload;
=======
use crate::tools::context::ToolOutput;
use crate::tools::context::boxed_tool_output;
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
use codex_protocol::protocol::McpInvocation;
use codex_tools::ToolExecutionStatus;
use codex_tools::ToolOutput;

mod list_mcp_resource_templates;
mod list_mcp_resources;
mod read_mcp_resource;

pub use list_mcp_resource_templates::ListMcpResourceTemplatesHandler;
pub use list_mcp_resources::ListMcpResourcesHandler;
pub use read_mcp_resource::ReadMcpResourceHandler;

fn model_can_access_mcp_server(turn: &TurnContext, server: &str) -> bool {
    turn.config.orchestrator_mcp_enabled || server != CODEX_APPS_MCP_SERVER_NAME
}

fn ensure_model_can_access_mcp_server(
    turn: &TurnContext,
    server: &str,
) -> Result<(), FunctionCallError> {
    if model_can_access_mcp_server(turn, server) {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "MCP server '{server}' is disabled by `orchestrator.mcp.enabled`"
        )))
    }
}

#[derive(Debug, Deserialize, Default, PartialEq, Eq)]
struct ListResourceArgs {
    #[serde(default)]
    server: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
}

impl ListResourceArgs {
    fn normalized(self) -> Self {
        Self {
            server: normalize_optional_string(self.server),
            cursor: normalize_optional_string(self.cursor),
        }
    }

    fn target(
        &self,
        turn: &TurnContext,
    ) -> Result<Option<(String, Option<PaginatedRequestParams>)>, FunctionCallError> {
        match &self.server {
            Some(server) => {
                ensure_model_can_access_mcp_server(turn, server)?;
                let params = self
                    .cursor
                    .clone()
                    .map(|cursor| PaginatedRequestParams::default().with_cursor(Some(cursor)));
                Ok(Some((server.clone(), params)))
            }
            None if self.cursor.is_some() => Err(FunctionCallError::RespondToModel(
                "cursor can only be used when a server is specified".to_string(),
            )),
            None => Ok(None),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ReadResourceArgs {
    server: String,
    uri: String,
}

#[derive(Debug, Serialize)]
struct ResourceWithServer<T> {
    server: String,
    #[serde(flatten)]
    resource: T,
}

impl<T> ResourceWithServer<T> {
    fn new(server: String, resource: T) -> Self {
        Self { server, resource }
    }

    fn from_server(server: &str, resources: Vec<T>) -> Vec<Self> {
        resources
            .into_iter()
            .map(|resource| Self::new(server.to_string(), resource))
            .collect()
    }

    fn from_all_servers(resources_by_server: HashMap<String, Vec<T>>) -> Vec<Self> {
        let mut entries: Vec<_> = resources_by_server.into_iter().collect();
        entries.sort_by(|(left, _), (right, _)| left.cmp(right));
        entries
            .into_iter()
            .flat_map(|(server, resources)| Self::from_server(&server, resources))
            .collect()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResourcesPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    resources: Vec<ResourceWithServer<Resource>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failures: Vec<McpResourceListingFailure>,
}

impl ListResourcesPayload {
    fn from_single_server(server: String, result: ListResourcesResult) -> Self {
        Self {
            resources: ResourceWithServer::from_server(&server, result.resources),
            server: Some(server),
            next_cursor: result.next_cursor,
            failures: Vec::new(),
        }
    }

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    fn from_all_servers(listing: McpResourceListing<Resource>) -> Self {
        let McpResourceListing {
            by_server: resources_by_server,
            failures,
        } = listing;
        let mut entries: Vec<(String, Vec<Resource>)> = resources_by_server.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut resources = Vec::new();
        for (server, server_resources) in entries {
            for resource in server_resources {
                resources.push(ResourceWithServer::new(server.clone(), resource));
            }
        }

=======
    fn from_all_servers(resources_by_server: HashMap<String, Vec<Resource>>) -> Self {
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
        Self {
            server: None,
            resources: ResourceWithServer::from_all_servers(resources_by_server),
            next_cursor: None,
            failures,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResourceTemplatesPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    resource_templates: Vec<ResourceWithServer<ResourceTemplate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failures: Vec<McpResourceListingFailure>,
}

impl ListResourceTemplatesPayload {
    fn from_single_server(server: String, result: ListResourceTemplatesResult) -> Self {
        Self {
            resource_templates: ResourceWithServer::from_server(&server, result.resource_templates),
            server: Some(server),
            next_cursor: result.next_cursor,
            failures: Vec::new(),
        }
    }

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
    fn from_all_servers(listing: McpResourceListing<ResourceTemplate>) -> Self {
        let McpResourceListing {
            by_server: templates_by_server,
            failures,
        } = listing;
        let mut entries: Vec<(String, Vec<ResourceTemplate>)> =
            templates_by_server.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let mut resource_templates = Vec::new();
        for (server, server_templates) in entries {
            for template in server_templates {
                resource_templates.push(ResourceTemplateWithServer::new(server.clone(), template));
            }
        }

=======
    fn from_all_servers(templates_by_server: HashMap<String, Vec<ResourceTemplate>>) -> Self {
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
        Self {
            server: None,
            resource_templates: ResourceWithServer::from_all_servers(templates_by_server),
            next_cursor: None,
            failures,
        }
    }
}

#[derive(Debug, Serialize)]
struct ReadResourcePayload {
    server: String,
    uri: String,
    #[serde(flatten)]
    result: ReadResourceResult,
}

/// Separates the model-safe result from the complete resource payload that code mode exposes.
///
/// `read_mcp_resource` is an unusual built-in: an MCP resource can itself be structured data.
/// Applying a generic middle truncation to its enclosing JSON string can make a declared JSON
/// resource invalid. The model receives either the complete serialized payload or a small,
/// explicit error. Code mode retains the unbounded resource payload, matching its pre-bounding
/// result shape.
struct ReadResourceToolOutput {
    model_output: FunctionToolOutput,
    raw_content: String,
}

impl ReadResourceToolOutput {
    fn model_content(&self) -> String {
        function_call_output_content_items_to_text(&self.model_output.body).unwrap_or_default()
    }

    #[cfg(test)]
    fn model_success(&self) -> Option<bool> {
        self.model_output.success
    }

    fn execution_status_for_source(
        &self,
        source: &crate::tools::context::ToolCallSource,
    ) -> ToolExecutionStatus {
        match source {
            crate::tools::context::ToolCallSource::Direct => {
                ToolExecutionStatus::from_success(self.success_for_logging())
            }
            crate::tools::context::ToolCallSource::CodeMode { .. } => {
                ToolExecutionStatus::Completed
            }
        }
    }
}

impl ToolOutput for ReadResourceToolOutput {
    fn log_preview(&self) -> String {
        self.model_output.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.model_output.success_for_logging()
    }

    fn code_mode_execution_status(&self) -> ToolExecutionStatus {
        ToolExecutionStatus::Completed
    }

    fn to_response_item(
        &self,
        call_id: &str,
        payload: &ToolPayload,
    ) -> codex_protocol::models::ResponseInputItem {
        self.model_output.to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> Value {
        Value::String(self.raw_content.clone())
    }
}

// This is deliberately a fixed string rather than a serialized subset of the
// resource metadata. A server name and resource URI are supplied by an MCP
// server and may be arbitrarily large. Including either in the model-facing
// error would let an otherwise bounded failure exceed the history cap.
const BOUNDED_JSON_RESOURCE_MODEL_ERROR: &str = r#"{"error":{"code":"mcp_resource_model_output_too_large","message":"The resource contains JSON that exceeds the model output limit.","truncated":true}}"#;

fn call_tool_result_from_content(content: &str, success: Option<bool>) -> CallToolResult {
    CallToolResult {
        content: vec![serde_json::json!({"type": "text", "text": content})],
        structured_content: None,
        is_error: success.map(|value| !value),
        meta: None,
    }
}

fn call_tool_result_from_execution_status(
    content: &str,
    execution_status: ToolExecutionStatus,
) -> CallToolResult {
    CallToolResult {
        content: vec![serde_json::json!({"type": "text", "text": content})],
        structured_content: None,
        is_error: Some(!execution_status.is_completed()),
        meta: None,
    }
}

async fn emit_tool_call_begin(
    session: &Arc<Session>,
    turn: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
) {
    let McpInvocation {
        server,
        tool,
        arguments,
    } = invocation;
    let item = TurnItem::McpToolCall(McpToolCallItem {
        id: call_id.to_string(),
        server,
        tool,
        arguments: arguments.unwrap_or(Value::Null),
        connector_id: None,
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        link_id: None,
        app_name: None,
        action_name: None,
        plugin_id: None,
        read_only_hint: None,
        status: McpToolCallStatus::InProgress,
        result: None,
        error: None,
        duration: None,
    });
    session.emit_turn_item_started(turn, &item).await;
}

async fn emit_tool_call_end(
    session: &Arc<Session>,
    turn: &TurnContext,
    call_id: &str,
    invocation: McpInvocation,
    duration: Duration,
    result: Result<CallToolResult, String>,
) {
    let (status, result, error) = match result {
        Ok(result) if result.is_error.unwrap_or(false) => {
            (McpToolCallStatus::Failed, Some(result), None)
        }
        Ok(result) => (McpToolCallStatus::Completed, Some(result), None),
        Err(message) => (
            McpToolCallStatus::Failed,
            None,
            Some(McpToolCallError { message }),
        ),
    };
    let McpInvocation {
        server,
        tool,
        arguments,
    } = invocation;
    let item = TurnItem::McpToolCall(McpToolCallItem {
        id: call_id.to_string(),
        server,
        tool,
        arguments: arguments.unwrap_or(Value::Null),
        connector_id: None,
        mcp_app_resource_uri: None,
        mcp_app_ui: None,
        link_id: None,
        app_name: None,
        action_name: None,
        plugin_id: None,
        read_only_hint: None,
        status,
        result,
        error,
        duration: Some(duration),
    });
    session.emit_turn_item_completed(turn, item).await;
}

async fn run_resource_operation<T>(
    session: &Arc<Session>,
    step_context: &StepContext,
    call_id: &str,
    invocation: McpInvocation,
    operation: impl Future<Output = Result<T, FunctionCallError>>,
) -> Result<Box<dyn ToolOutput>, FunctionCallError>
where
    T: Serialize,
{
    let turn = &step_context.turn;
    emit_tool_call_begin(session, turn, call_id, invocation.clone()).await;
    let start = Instant::now();
    let result = operation.await.and_then(|payload| {
        serialize_function_output(
            payload,
            step_context.settings.model_info.truncation_policy.into(),
        )
    });

    match result {
        Ok(output) => {
            let content =
                function_call_output_content_items_to_text(&output.body).unwrap_or_default();
            emit_tool_call_end(
                session,
                turn,
                call_id,
                invocation,
                start.elapsed(),
                Ok(call_tool_result_from_content(&content, output.success)),
            )
            .await;
            Ok(boxed_tool_output(output))
        }
        Err(error) => {
            emit_tool_call_end(
                session,
                turn,
                call_id,
                invocation,
                start.elapsed(),
                Err(error.to_string()),
            )
            .await;
            Err(error)
        }
    }
}

fn normalize_optional_string(input: Option<String>) -> Option<String> {
    input.and_then(|value| {
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    })
}

fn normalize_required_string(field: &str, value: String) -> Result<String, FunctionCallError> {
    match normalize_optional_string(Some(value)) {
        Some(normalized) => Ok(normalized),
        None => Err(FunctionCallError::RespondToModel(format!(
            "{field} must be provided"
        ))),
    }
}

fn serialize_function_output<T>(
    payload: T,
    truncation_policy: TruncationPolicy,
) -> Result<FunctionToolOutput, FunctionCallError>
where
    T: Serialize,
{
    let content = serde_json::to_string(&payload).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to serialize MCP resource response: {err}"
        ))
    })?;
    // Match regular MCP tool outputs by bounding the copy persisted to the
    // rollout and injected into model context.
    let content = truncate_text(&content, with_serialization_allowance(truncation_policy));

    Ok(FunctionToolOutput::from_text(content, Some(true)))
}

fn serialize_read_resource_output(
    payload: ReadResourcePayload,
    truncation_policy: TruncationPolicy,
) -> Result<ReadResourceToolOutput, FunctionCallError> {
    let raw_content = serde_json::to_string(&payload).map_err(|err| {
        FunctionCallError::RespondToModel(format!(
            "failed to serialize MCP resource response: {err}"
        ))
    })?;
    let bounded_content = truncate_text(&raw_content, truncation_policy * 1.2);
    let requires_structured_failure = bounded_content != raw_content
        && payload.result.contents.iter().any(|content| {
            matches!(
                content,
                rmcp::model::ResourceContents::TextResourceContents {
                    mime_type: Some(mime_type),
                    ..
                } if is_json_media_type(mime_type)
            )
        });

    let model_output = if requires_structured_failure {
        FunctionToolOutput::from_text(BOUNDED_JSON_RESOURCE_MODEL_ERROR.to_string(), Some(false))
    } else {
        FunctionToolOutput::from_text(bounded_content, Some(true))
    };

    Ok(ReadResourceToolOutput {
        model_output,
        raw_content,
    })
}

fn is_json_media_type(mime_type: &str) -> bool {
    let essence = mime_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();

    essence == "application/json"
        || essence.strip_prefix("application/").is_some_and(|subtype| {
            subtype
                .strip_suffix("+json")
                .is_some_and(|prefix| !prefix.is_empty())
        })
}

fn parse_arguments(raw_args: &str) -> Result<Option<Value>, FunctionCallError> {
    if raw_args.trim().is_empty() {
        Ok(None)
    } else {
        let value: Value = serde_json::from_str(raw_args).map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
        })?;
        if value.is_null() {
            Ok(None)
        } else {
            Ok(Some(value))
        }
    }
}

fn parse_args<T>(arguments: Option<Value>) -> Result<T, FunctionCallError>
where
    T: DeserializeOwned,
{
    match arguments {
        Some(value) => serde_json::from_value(value).map_err(|err| {
            FunctionCallError::RespondToModel(format!("failed to parse function arguments: {err}"))
        }),
        None => Err(FunctionCallError::RespondToModel(
            "failed to parse function arguments: expected value".to_string(),
        )),
    }
}

fn parse_args_with_default<T>(arguments: Option<Value>) -> Result<T, FunctionCallError>
where
    T: DeserializeOwned + Default,
{
    match arguments {
        Some(value) => parse_args(Some(value)),
        None => Ok(T::default()),
    }
}

#[cfg(test)]
#[path = "mcp_resource_tests.rs"]
mod tests;
