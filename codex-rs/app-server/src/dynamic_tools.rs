use codex_app_server_protocol::DynamicToolCallOutputContentItem;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_core::CodexThread;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem as CoreDynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolResponse as CoreDynamicToolResponse;
use codex_protocol::protocol::Op;
use std::env;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::PathBuf;
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
    let (response, response_accepted) = match response {
        Ok(Ok(value)) => {
            let (response, error) = decode_response(value);
            (response, error.is_none())
        }
        Ok(Err(err)) if is_turn_transition_server_request_error(&err) => {
            record_browser_app_server_stage(
                &call_id,
                /*response_accepted*/ false,
                /*response_submitted*/ false,
                /*accepted_item_count*/ 0,
            );
            return;
        }
        Ok(Err(err)) => {
            error!("request failed with client error: {err:?}");
            (fallback_response("dynamic tool request failed").0, false)
        }
        Err(err) => {
            error!("request failed: {err:?}");
            (fallback_response("dynamic tool request failed").0, false)
        }
    };

    let accepted_item_count = if response_accepted {
        response.content_items.len()
    } else {
        0
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
    let submit_result = conversation
        .submit(Op::DynamicToolResponse {
            id: call_id.clone(),
            response: core_response,
        })
        .await;
    let response_submitted = submit_result.is_ok();
    if let Err(err) = submit_result {
        error!("failed to submit DynamicToolResponse: {err}");
    }
    record_browser_app_server_stage(
        &call_id,
        response_accepted,
        response_submitted,
        accepted_item_count,
    );
}

fn record_browser_app_server_stage(
    call_id: &str,
    response_accepted: bool,
    response_submitted: bool,
    accepted_item_count: usize,
) {
    if env::var("CODEX_TEST_BROWSER_OUTPUT_DIAGNOSTIC")
        .ok()
        .as_deref()
        != Some("1")
    {
        return;
    }
    let (Ok(expected_call_id), Some(codex_home)) = (
        env::var("CODEX_TEST_BROWSER_OUTPUT_DIAGNOSTIC_CALL_ID"),
        env::var_os("CODEX_HOME"),
    ) else {
        return;
    };
    let path = PathBuf::from(codex_home).join("browser-output-stage-diagnostic.jsonl");
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let observation = serde_json::json!({
        "stage": "app_server_response",
        "call_id_matches_fixture": call_id == expected_call_id.as_str(),
        "app_server_response_accepted": response_accepted,
        "app_server_response_submitted": response_submitted,
        "app_server_accepted_item_count": accepted_item_count,
    });
    if let Ok(mut line) = serde_json::to_vec(&observation) {
        line.push(b'\n');
        let _ = file.write_all(&line);
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
