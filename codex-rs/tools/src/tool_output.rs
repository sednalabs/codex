use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseInputItem;
use serde_json::Value as JsonValue;

use crate::ToolPayload;

<<<<<<< f12747ca5e6eb85d32a823b9450726c76ffbb93e
const TELEMETRY_PREVIEW_MAX_BYTES: usize = 2 * 1024;
const TELEMETRY_PREVIEW_MAX_LINES: usize = 64;
const TELEMETRY_PREVIEW_TRUNCATION_NOTICE: &str = "[... telemetry preview truncated ...]";

/// The execution status recorded for a tool invocation.
///
/// This is deliberately separate from the model-facing success bit. Most
/// tools use the same result for both callers, but Code Mode can safely
/// consume a complete runtime value while the model must receive a bounded
/// failed projection of that same value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolExecutionStatus {
    Completed,
    Failed,
}

impl ToolExecutionStatus {
    pub fn from_success(success: bool) -> Self {
        if success {
            Self::Completed
        } else {
            Self::Failed
        }
    }

    pub fn is_completed(self) -> bool {
        matches!(self, Self::Completed)
    }
}

=======
>>>>>>> 7f83d4922d7e92a36c1c1e4f61159a5815d45360
/// Model-facing output contract returned by executable tool runtimes.
pub trait ToolOutput: Send {
    /// Returns a deliberately lossy diagnostic representation, before telemetry size limits.
    /// Implementations may summarize results or omit media and encrypted content. This is not
    /// the authoritative tool result; an untruncated log does not imply a complete result.
    /// The logger owns the additional configurable byte limit.
    fn log_output(&self) -> String;

    fn success_for_logging(&self) -> bool;

    /// Returns the execution result when this output is consumed by Code Mode.
    ///
    /// The default preserves the ordinary model-facing result. Outputs that
    /// intentionally fail-close a model projection while retaining a complete
    /// Code Mode value may report `Completed` here instead.
    fn code_mode_execution_status(&self) -> ToolExecutionStatus {
        ToolExecutionStatus::from_success(self.success_for_logging())
    }

    /// Whether this output contains external context that should disable memory generation when
    /// `memories.disable_on_external_context` is enabled.
    fn contains_external_context(&self) -> bool {
        false
    }

    /// Overrides history's fallback token limit after tool-specific truncation.
    /// Include any serialization allowance; history uses this limit unchanged.
    fn fallback_token_limit_override(&self) -> Option<usize> {
        None
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem;

    /// Returns the tool call id exposed to `PostToolUse` hooks for this output.
    fn post_tool_use_id(&self, call_id: &str) -> String {
        call_id.to_string()
    }

    /// Returns the tool input exposed to `PostToolUse` hooks for this output.
    fn post_tool_use_input(&self, _payload: &ToolPayload) -> Option<JsonValue> {
        None
    }

    /// Returns the stable value exposed to `PostToolUse` hooks for this tool output.
    ///
    /// Tool handlers decide whether a tool participates in `PostToolUse`, but
    /// this method lets the output type own any conversion from model-facing
    /// response content to hook-facing data. Returning `None` means the output
    /// should not produce a post-use hook payload, not merely that the tool had
    /// empty output.
    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        None
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> JsonValue {
        response_input_to_code_mode_result(self.to_response_item("", payload))
    }

    /// Borrows original host-only metadata for recording, not for model output or logging.
    fn tool_result_metadata(&self) -> Option<&JsonValue> {
        None
    }
}

impl<T> ToolOutput for Box<T>
where
    T: ToolOutput + ?Sized,
{
    fn log_output(&self) -> String {
        (**self).log_output()
    }

    fn success_for_logging(&self) -> bool {
        (**self).success_for_logging()
    }

    fn code_mode_execution_status(&self) -> ToolExecutionStatus {
        (**self).code_mode_execution_status()
    }

    fn contains_external_context(&self) -> bool {
        (**self).contains_external_context()
    }

    fn fallback_token_limit_override(&self) -> Option<usize> {
        (**self).fallback_token_limit_override()
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        (**self).to_response_item(call_id, payload)
    }

    fn post_tool_use_id(&self, call_id: &str) -> String {
        (**self).post_tool_use_id(call_id)
    }

    fn post_tool_use_input(&self, payload: &ToolPayload) -> Option<JsonValue> {
        (**self).post_tool_use_input(payload)
    }

    fn post_tool_use_response(&self, call_id: &str, payload: &ToolPayload) -> Option<JsonValue> {
        (**self).post_tool_use_response(call_id, payload)
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> JsonValue {
        (**self).code_mode_result(payload)
    }

    fn tool_result_metadata(&self) -> Option<&JsonValue> {
        (**self).tool_result_metadata()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct JsonToolOutput {
    value: JsonValue,
    success: Option<bool>,
    contains_external_context: bool,
}

impl JsonToolOutput {
    pub fn new(value: JsonValue) -> Self {
        Self {
            value,
            success: Some(true),
            contains_external_context: false,
        }
    }

    pub fn with_success(value: JsonValue, success: Option<bool>) -> Self {
        Self {
            value,
            success,
            contains_external_context: false,
        }
    }

    pub fn with_external_context(mut self) -> Self {
        self.contains_external_context = true;
        self
    }
}

impl ToolOutput for JsonToolOutput {
    fn log_output(&self) -> String {
        self.value.to_string()
    }

    fn success_for_logging(&self) -> bool {
        self.success.unwrap_or(true)
    }

    fn contains_external_context(&self) -> bool {
        self.contains_external_context
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        let output = FunctionCallOutputPayload {
            body: FunctionCallOutputBody::Text(self.value.to_string()),
            success: self.success,
        };

        if matches!(payload, ToolPayload::Custom { .. }) {
            return ResponseInputItem::CustomToolCallOutput {
                call_id: call_id.to_string(),
                name: None,
                output,
            };
        }

        ResponseInputItem::FunctionCallOutput {
            call_id: call_id.to_string(),
            output,
        }
    }

    fn post_tool_use_response(&self, _call_id: &str, _payload: &ToolPayload) -> Option<JsonValue> {
        Some(self.value.clone())
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        self.value.clone()
    }
}

impl ToolOutput for codex_protocol::mcp::CallToolResult {
    fn log_output(&self) -> String {
        let output = self.as_function_call_output_payload();
        // Do not fall back to serializing media or encrypted content into logs.
        output.body.to_text().unwrap_or_default()
    }

    fn success_for_logging(&self) -> bool {
        self.success()
    }

    fn to_response_item(&self, call_id: &str, _payload: &ToolPayload) -> ResponseInputItem {
        ResponseInputItem::McpToolCallOutput {
            call_id: call_id.to_string(),
            output: self.clone(),
        }
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> JsonValue {
        let mut result = serde_json::to_value(self).unwrap_or_else(|err| {
            JsonValue::String(format!("failed to serialize mcp result: {err}"))
        });
        // MCP result metadata is private to clients and must not reach Code Mode.
        if let JsonValue::Object(fields) = &mut result {
            fields.remove("_meta");
        }
        result
    }
}

fn response_input_to_code_mode_result(response: ResponseInputItem) -> JsonValue {
    match response {
        ResponseInputItem::Message { content, .. } => content_items_to_code_mode_result(
            &content
                .into_iter()
                .map(|item| match item {
                    codex_protocol::models::ContentItem::InputText { text }
                    | codex_protocol::models::ContentItem::OutputText { text } => {
                        FunctionCallOutputContentItem::InputText { text }
                    }
                    codex_protocol::models::ContentItem::InputImage { image, detail } => {
                        FunctionCallOutputContentItem::InputImage {
                            image,
                            detail: detail.or(Some(DEFAULT_IMAGE_DETAIL)),
                        }
                    }
                    codex_protocol::models::ContentItem::InputAudio { audio_url } => {
                        FunctionCallOutputContentItem::InputAudio { audio_url }
                    }
                })
                .collect::<Vec<_>>(),
        ),
        ResponseInputItem::FunctionCallOutput { output, .. }
        | ResponseInputItem::CustomToolCallOutput { output, .. } => match output.body {
            FunctionCallOutputBody::Text(text) => JsonValue::String(text),
            FunctionCallOutputBody::ContentItems(items) => {
                content_items_to_code_mode_result(&items)
            }
        },
        ResponseInputItem::ToolSearchOutput { tools, .. } => JsonValue::Array(tools),
        ResponseInputItem::McpToolCallOutput { output, .. } => serde_json::to_value(output)
            .unwrap_or_else(|err| {
                JsonValue::String(format!("failed to serialize mcp result: {err}"))
            }),
    }
}

fn content_items_to_code_mode_result(items: &[FunctionCallOutputContentItem]) -> JsonValue {
    JsonValue::String(
        items
            .iter()
            .filter_map(|item| match item {
                FunctionCallOutputContentItem::InputText { text } if !text.trim().is_empty() => {
                    Some(text.clone())
                }
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::Inline { image_url },
                    ..
                } if !image_url.trim().is_empty() => Some(image_url.clone()),
                FunctionCallOutputContentItem::InputImage {
                    image: ImageReference::File { file_id },
                    ..
                } if !file_id.trim().is_empty() => Some(file_id.clone()),
                FunctionCallOutputContentItem::InputAudio { audio_url }
                    if !audio_url.trim().is_empty() =>
                {
                    Some(audio_url.clone())
                }
                FunctionCallOutputContentItem::InputText { .. }
                | FunctionCallOutputContentItem::InputImage { .. }
                | FunctionCallOutputContentItem::InputAudio { .. }
                | FunctionCallOutputContentItem::EncryptedContent { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

#[cfg(test)]
#[path = "tool_output_tests.rs"]
mod tests;
