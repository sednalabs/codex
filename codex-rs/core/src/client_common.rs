pub use codex_api::ResponseEvent;
use codex_protocol::error::Result;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ContentItem;
use codex_protocol::models::DEFAULT_IMAGE_DETAIL;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelInfo;
use codex_tools::ToolSpec;
use futures::Stream;
use serde_json::Value;
use std::env;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// API request payload for a single model turn
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Conversation context input items.
    pub input: Vec<ResponseItem>,

    /// Tool definitions to inject into this request, empty when supplied by history.
    pub(crate) tools: Arc<[ToolSpec]>,

    /// Whether parallel tool calls are permitted for this prompt.
    pub(crate) parallel_tool_calls: bool,

    pub base_instructions: BaseInstructions,

    /// Optional the output schema for the model's response.
    pub output_schema: Option<Value>,

    /// Whether the Responses API should strictly validate `output_schema`.
    pub output_schema_strict: bool,

    pub(crate) cyber_access_program: Option<codex_protocol::turn_input::CyberAccessProgram>,
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            input: Vec::new(),
            tools: Arc::default(),
            parallel_tool_calls: false,
            base_instructions: BaseInstructions::default(),
            output_schema: None,
            output_schema_strict: true,
            cyber_access_program: None,
        }
    }
}

impl Prompt {
    pub(crate) fn get_formatted_input_for_request(
        &self,
        model_info: &ModelInfo,
    ) -> Vec<ResponseItem> {
        let mut input = self.input.clone();
        normalize_image_details(&mut input, model_info);
        input
    }
}

pub(crate) fn record_browser_output_stage<'a>(
    stage: &str,
    items: impl IntoIterator<Item = &'a ResponseItem>,
) {
    if env::var("CODEX_TEST_BROWSER_OUTPUT_DIAGNOSTIC")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let (Ok(expected_call_id), Some(codex_home)) = (
        env::var("CODEX_TEST_BROWSER_OUTPUT_DIAGNOSTIC_CALL_ID"),
        env::var_os("CODEX_HOME"),
    ) else {
        return;
    };

    let mut function_call_count = 0usize;
    let mut function_call_output_count = 0usize;
    for item in items {
        match item {
            ResponseItem::FunctionCall { call_id, .. } if call_id == &expected_call_id => {
                function_call_count += 1;
            }
            ResponseItem::FunctionCallOutput {
                call_id: Some(call_id),
                ..
            } if call_id == &expected_call_id => {
                function_call_output_count += 1;
            }
            _ => {}
        }
    }

    let path = PathBuf::from(codex_home).join("browser-output-stage-diagnostic.jsonl");
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let observation = serde_json::json!({
        "stage": stage,
        "call_id_matches_fixture": function_call_count > 0 || function_call_output_count > 0,
        "function_call_count": function_call_count,
        "function_call_output_count": function_call_output_count,
    });
    if let Ok(mut line) = serde_json::to_vec(&observation) {
        line.push(b'\n');
        let _ = file.write_all(&line);
    }
}

fn normalize_image_details(items: &mut [ResponseItem], model_info: &ModelInfo) {
    for item in items {
        match item {
            ResponseItem::Message { content, .. } => {
                for content_item in content {
                    if let ContentItem::InputImage { detail, .. } = content_item {
                        normalize_image_detail(detail, model_info);
                    }
                }
            }
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => {
                if let Some(content) = output.content_items_mut() {
                    for content_item in content {
                        if let FunctionCallOutputContentItem::InputImage { detail, .. } =
                            content_item
                        {
                            normalize_image_detail(detail, model_info);
                        }
                    }
                }
            }
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ConfigurationUpdate { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => {}
        }
    }
}

fn normalize_image_detail(detail: &mut Option<ImageDetail>, model_info: &ModelInfo) {
    if model_info.use_responses_lite {
        *detail = None;
    } else if *detail == Some(ImageDetail::Original) && !model_info.supports_image_detail_original {
        *detail = Some(DEFAULT_IMAGE_DETAIL);
    }
}

pub struct ResponseStream {
    pub(crate) rx_event: mpsc::Receiver<Result<ResponseEvent>>,
    pub(crate) interrupt: Option<oneshot::Sender<()>>,
    /// Signals the mapper task that the consumer stopped polling before the
    /// provider stream reached its own terminal event.
    pub(crate) consumer_dropped: CancellationToken,
}

impl Stream for ResponseStream {
    type Item = Result<ResponseEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx_event.poll_recv(cx)
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        self.consumer_dropped.cancel();
    }
}

#[cfg(test)]
#[path = "client_common_tests.rs"]
mod tests;
