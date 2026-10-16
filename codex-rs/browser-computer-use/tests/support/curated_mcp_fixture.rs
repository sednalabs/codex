use serde_json::Value;
use serde_json::json;

pub const IMAGE_DATA_URLS: [&str; 2] = [
    "data:image/png;base64,AQID",
    "data:image/png;base64,BAUG",
];

pub fn image_tool_response(request_id: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {
            "content": [
                {"type": "text", "text": "synthetic visual result"},
                {"type": "image", "data": "AQID", "mimeType": "image/png"},
                {"type": "image", "data": "BAUG", "mimeType": "image/png"}
            ],
            "isError": false
        }
    })
}
