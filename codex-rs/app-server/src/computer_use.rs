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
    let (response, response_error) = match response {
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
    } = response;
    let core_response = CoreComputerUseResponse {
        content_items: content_items
            .into_iter()
            .map(codex_protocol::computer_use::ComputerUseOutputContentItem::from)
            .collect(),
        success,
        error: response_error,
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

fn decode_response(value: serde_json::Value) -> (ComputerUseCallResponse, Option<String>) {
    match serde_json::from_value::<ComputerUseCallResponse>(value) {
        Ok(response) => (response, None),
        Err(err) => {
            error!("failed to deserialize ComputerUseCallResponse: {err}");
            fallback_response("computer-use response was invalid")
        }
    }
}

fn fallback_response(message: &str) -> (ComputerUseCallResponse, Option<String>) {
    (
        ComputerUseCallResponse {
            content_items: vec![ComputerUseCallOutputContentItem::InputText {
                text: message.to_string(),
            }],
            success: false,
        },
        Some(message.to_string()),
    )
}
