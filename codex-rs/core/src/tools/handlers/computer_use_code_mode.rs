use codex_protocol::models::FunctionCallOutputContentItem;
use serde_json::Value;
use serde_json::json;

pub(super) fn output_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "content": {
                "type": "array",
                "items": {
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {
                                "type": { "const": "input_text" },
                                "text": { "type": "string" }
                            },
                            "required": ["type", "text"],
                            "additionalProperties": false
                        },
                        {
                            "type": "object",
                            "properties": {
                                "type": { "const": "input_image" },
                                "image_url": { "type": "string" },
                                "detail": {
                                    "enum": ["auto", "low", "high", "original"]
                                }
                            },
                            "required": ["type", "image_url"],
                            "additionalProperties": false
                        }
                    ]
                }
            },
            "success": { "type": "boolean" }
        },
        "required": ["content", "success"],
        "additionalProperties": false
    })
}

pub(super) fn result(content: &[FunctionCallOutputContentItem], success: bool) -> Value {
    json!({
        "content": content,
        "success": success,
    })
}
