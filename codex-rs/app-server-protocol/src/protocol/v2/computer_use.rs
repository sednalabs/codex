use crate::JsonSchema;
use crate::TS;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

/// Parameters for a native computer-use call delegated to the client.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ComputerUseCallParams {
    pub thread_id: String,
    pub turn_id: String,
    pub call_id: String,
    pub environment_id: Option<String>,
    pub adapter: String,
    pub tool: String,
    pub arguments: JsonValue,
}

/// Result returned by a native computer-use caller.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ComputerUseCallResponse {
    pub content_items: Vec<ComputerUseCallOutputContentItem>,
    pub success: bool,
}

/// Content emitted by a native computer-use caller.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
#[ts(export_to = "v2/")]
pub enum ComputerUseCallOutputContentItem {
    InputText { text: String },
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        detail: Option<String>,
    },
}
