use codex_app_server_protocol::ComputerUseCallOutputContentItem;
use codex_app_server_protocol::ComputerUseCallResponse;
use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_core::CodexThread;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem as CoreDynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolResponse as CoreDynamicToolResponse;
use codex_protocol::protocol::Op;
use std::sync::Arc;
use tokio::sync::oneshot;
use tracing::error;

use crate::image_url::REMOTE_IMAGE_URL_ERROR;
use crate::image_url::is_remote_image_url;
use crate::outgoing_message::ClientRequestResult;
use crate::server_request_error::is_turn_transition_server_request_error;

const INVALID_AUDIO_URL_ERROR: &str = "audio URLs must use an inline data URL";

pub(crate) async fn on_call_response(
    call_id: String,
    receiver: oneshot::Receiver<ClientRequestResult>,
    conversation: Arc<CodexThread>,
) {
    let response = receiver.await;
    let (response, _error) = match response {
        Ok(Ok(value)) => decode_response(value),
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => return,
        Ok(Err(err)) => {
            error!("request failed with client error: {err:?}");
            fallback_response("dynamic tool request failed")
        }
        Err(err) => {
            error!("request failed: {err:?}");
            fallback_response("dynamic tool request failed")
        }
    };

    let DynamicToolCallResponse {
        content_items,
        success,
    } = response.clone();
    let core_response = CoreDynamicToolResponse {
        content_items: content_items
            .into_iter()
            .map(CoreDynamicToolCallOutputContentItem::from)
            .collect(),
        success,
    };
    if let Err(err) = conversation
        .submit(Op::DynamicToolResponse {
            id: call_id.clone(),
            response: core_response,
        })
        .await
    {
        error!("failed to submit DynamicToolResponse: {err}");
    }
}

/// Resolve a browser dynamic-tool call returned through the typed
/// computer-use request path.
pub(crate) async fn on_computer_use_response(
    call_id: String,
    receiver: oneshot::Receiver<ClientRequestResult>,
    conversation: Arc<CodexThread>,
) {
    let response = match receiver.await {
        Ok(Ok(value)) => decode_computer_use_response(value),
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => return,
        Ok(Err(err)) => {
            error!("computer-use request failed with client error: {err:?}");
            fallback_response("computer-use request failed").0
        }
        Err(err) => {
            error!("computer-use request failed: {err:?}");
            fallback_response("computer-use request failed").0
        }
    };
    if let Err(err) = conversation
        .submit(Op::DynamicToolResponse {
            id: call_id,
            response: CoreDynamicToolResponse {
                content_items: response
                    .content_items
                    .into_iter()
                    .map(CoreDynamicToolCallOutputContentItem::from)
                    .collect(),
                success: response.success,
            },
        })
        .await
    {
        error!("failed to submit computer-use DynamicToolResponse: {err}");
    }
}

fn decode_computer_use_response(value: serde_json::Value) -> DynamicToolCallResponse {
    match serde_json::from_value::<ComputerUseCallResponse>(value) {
        Ok(response) => {
            let response = DynamicToolCallResponse {
                content_items: response
                    .content_items
                    .into_iter()
                    .map(|item| match item {
                        ComputerUseCallOutputContentItem::InputText { text } => {
                            DynamicToolCallOutputContentItem::InputText { text }
                        }
                        ComputerUseCallOutputContentItem::InputImage { image_url, .. } => {
                            DynamicToolCallOutputContentItem::InputImage { image_url }
                        }
                    })
                    .collect(),
                success: response.success,
            };
            if response.content_items.iter().any(|item| {
                matches!(
                    item,
                    DynamicToolCallOutputContentItem::InputImage { image_url }
                        if is_remote_image_url(image_url)
                )
            }) {
                error!(
                    message = REMOTE_IMAGE_URL_ERROR,
                    "computer-use response was invalid"
                );
                fallback_response(REMOTE_IMAGE_URL_ERROR).0
            } else {
                response
            }
        }
        Err(err) => {
            error!("failed to deserialize ComputerUseCallResponse: {err}");
            fallback_response("computer-use response was invalid").0
        }
    }
}

fn decode_response(value: serde_json::Value) -> (DynamicToolCallResponse, Option<String>) {
    match serde_json::from_value::<DynamicToolCallResponse>(value) {
        Ok(response)
            if response.content_items.iter().any(|item| {
                matches!(
                    item,
                    DynamicToolCallOutputContentItem::InputImage { image_url }
                        if is_remote_image_url(image_url)
                )
            }) =>
        {
            error!(
                message = REMOTE_IMAGE_URL_ERROR,
                "dynamic tool response was invalid"
            );
            fallback_response(REMOTE_IMAGE_URL_ERROR)
        }
        Ok(response)
            if response.content_items.iter().any(|item| {
                matches!(
                    item,
                    DynamicToolCallOutputContentItem::InputAudio { audio_url }
                        if !audio_url
                            .get(.."data:".len())
                            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("data:"))
                )
            }) =>
        {
            error!(
                message = INVALID_AUDIO_URL_ERROR,
                "dynamic tool response was invalid"
            );
            fallback_response(INVALID_AUDIO_URL_ERROR)
        }
        Ok(response) => (response, None),
        Err(err) => {
            error!("failed to deserialize DynamicToolCallResponse: {err}");
            fallback_response("dynamic tool response was invalid")
        }
    }
}

fn fallback_response(message: &str) -> (DynamicToolCallResponse, Option<String>) {
    (
        DynamicToolCallResponse {
            content_items: vec![DynamicToolCallOutputContentItem::InputText {
                text: message.to_string(),
            }],
            success: false,
        },
        Some(message.to_string()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn computer_use_response_rejects_remote_image_urls() {
        let response = decode_computer_use_response(json!({
            "contentItems": [
                {"type": "inputImage", "imageUrl": "https://example.com/image.png"}
            ],
            "success": true
        }));

        assert!(!response.success);
        assert!(matches!(
            response.content_items.as_slice(),
            [DynamicToolCallOutputContentItem::InputText { text }]
                if text == REMOTE_IMAGE_URL_ERROR
        ));
    }

    #[test]
    fn computer_use_response_preserves_inline_image_urls() {
        let response = decode_computer_use_response(json!({
            "contentItems": [
                {"type": "inputImage", "imageUrl": "data:image/png;base64,AAAA"}
            ],
            "success": true
        }));

        assert!(response.success);
        assert!(matches!(
            response.content_items.as_slice(),
            [DynamicToolCallOutputContentItem::InputImage { image_url }]
                if image_url == "data:image/png;base64,AAAA"
        ));
    }
}
