use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolOutput;
use codex_tools::ToolPayload;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::HistoryNotesToolOutput;

#[test]
fn preserves_encrypted_history_output() {
    let result = HistoryNotesToolOutput {
        result: json!({"encrypted_output": "enc_payload"}),
    }
    .to_response_item(
        "call-1",
        &ToolPayload::Function {
            arguments: "{}".to_string(),
        },
    );

    let ResponseInputItem::FunctionCallOutput { output, .. } = result else {
        panic!("expected function-call output");
    };
    assert_eq!(
        output.content_items(),
        Some(
            [FunctionCallOutputContentItem::EncryptedContent {
                encrypted_content: "enc_payload".to_string(),
            }]
            .as_slice()
        )
    );
}
