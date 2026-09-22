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
    #[serde(rename_all = "camelCase")]
    InputText { text: String },
    #[serde(rename_all = "camelCase")]
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        detail: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::ComputerUseCallOutputContentItem;

    #[test]
    fn input_image_uses_camel_case_wire_field() {
        let item = ComputerUseCallOutputContentItem::InputImage {
            image_url: "data:image/png;base64,abc".to_string(),
            detail: Some("high".to_string()),
        };

        let encoded = serde_json::to_value(&item).expect("computer-use item serializes");
        assert_eq!(encoded["imageUrl"], "data:image/png;base64,abc");
        assert!(encoded.get("image_url").is_none());

        let decoded: ComputerUseCallOutputContentItem =
            serde_json::from_value(encoded).expect("computer-use item deserializes");
        assert_eq!(decoded, item);
    }
}
