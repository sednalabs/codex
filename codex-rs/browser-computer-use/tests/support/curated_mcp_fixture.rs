use serde_json::Value;
use serde_json::json;

pub const IMAGE_DATA_URL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";
pub const IMAGE_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==";

pub fn image_tool_response(request_id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {
            "content": [
                {"type": "text", "text": "synthetic visual result"},
                {"type": "image", "data": IMAGE_BASE64, "mimeType": "image/png"}
            ],
            "isError": false
        }
    })
}
