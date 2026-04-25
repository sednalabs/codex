use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ComputerUseCallRequest {
    pub call_id: String,
    pub turn_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    pub adapter: String,
    pub tool: String,
    pub arguments: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct ComputerUseResponse {
    pub content_items: Vec<ComputerUseOutputContentItem>,
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
pub enum ComputerUseOutputContentItem {
    #[serde(rename_all = "camelCase")]
    InputText { text: String },
    #[serde(rename_all = "camelCase")]
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl From<ComputerUseOutputContentItem> for crate::dynamic_tools::DynamicToolCallOutputContentItem {
    fn from(item: ComputerUseOutputContentItem) -> Self {
        match item {
            ComputerUseOutputContentItem::InputText { text } => Self::InputText { text },
            ComputerUseOutputContentItem::InputImage { image_url, detail } => {
                Self::InputImage { image_url, detail }
            }
        }
    }
}
