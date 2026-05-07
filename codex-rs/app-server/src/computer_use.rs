use codex_app_server_protocol::ComputerUseCallOutputContentItem;
use codex_app_server_protocol::ComputerUseCallResponse;
use codex_core::CodexThread;
use codex_protocol::computer_use::ComputerUseResponse as CoreComputerUseResponse;
use codex_protocol::protocol::Op;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

use crate::outgoing_message::ClientRequestResult;
use crate::server_request_error::is_turn_transition_server_request_error;

pub(crate) async fn on_call_response(
    call_id: String,
    receiver: oneshot::Receiver<ClientRequestResult>,
    conversation: Arc<CodexThread>,
) {
    let response = receiver.await;
    let response = match response {
        Ok(Ok(value)) => decode_response(value),
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => return,
        Ok(Err(err)) => {
            error!("computer-use request failed with client error: {err:?}");
            fallback_response("computer-use request failed")
        }
        Err(err) => {
            error!("computer-use request failed: {err:?}");
            fallback_response("computer-use request failed")
        }
    };

    let ComputerUseCallResponse {
        content_items,
        success,
        error,
    } = response;
    let core_response = CoreComputerUseResponse {
        content_items: content_items
            .into_iter()
            .map(|item| match item {
                ComputerUseCallOutputContentItem::InputText { text } => {
                    codex_protocol::computer_use::ComputerUseOutputContentItem::InputText { text }
                }
                ComputerUseCallOutputContentItem::InputImage { image_url, detail } => {
                    codex_protocol::computer_use::ComputerUseOutputContentItem::InputImage {
                        image_url,
                        detail,
                    }
                }
            })
            .collect(),
        success,
        error,
    };
    if let Err(err) = conversation
        .submit(Op::ComputerUseResponse {
            id: call_id.clone(),
            response: core_response,
        })
        .await
    {
        error!("failed to submit ComputerUseResponse: {err}");
    }
}

fn decode_response(value: serde_json::Value) -> ComputerUseCallResponse {
    match serde_json::from_value::<ComputerUseCallResponse>(value) {
        Ok(response) => response,
        Err(err) => {
            error!("failed to deserialize ComputerUseCallResponse: {err}");
            fallback_response("computer-use response was invalid")
        }
    }
}

fn fallback_response(message: &str) -> ComputerUseCallResponse {
    ComputerUseCallResponse {
        content_items: vec![ComputerUseCallOutputContentItem::InputText {
            text: message.to_string(),
        }],
        success: false,
        error: Some(message.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::ComputerUseCallOutputContentItem;
    use super::ComputerUseCallResponse;
    use super::decode_response;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn decode_response_preserves_failure_error() {
        assert_eq!(
            decode_response(json!({
                "contentItems": [
                    {
                        "type": "inputText",
                        "text": "screen unavailable"
                    }
                ],
                "success": false,
                "error": "android session disconnected"
            })),
            ComputerUseCallResponse {
                content_items: vec![ComputerUseCallOutputContentItem::InputText {
                    text: "screen unavailable".to_string(),
                }],
                success: false,
                error: Some("android session disconnected".to_string()),
            }
        );
    }

    #[test]
    fn decode_response_accepts_legacy_response_without_error() {
        assert_eq!(
            decode_response(json!({
                "contentItems": [],
                "success": true
            })),
            ComputerUseCallResponse {
                content_items: Vec::new(),
                success: true,
                error: None,
            }
        );
    }
}
