//! Dynamic-tool handlers finish their item lifecycle when cancellation is signalled.

use super::*;
use crate::state::ActiveTurn;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::ImageReference;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn native_visual_code_mode_results_preserve_typed_content_and_success() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    let body = vec![
        FunctionCallOutputContentItem::InputText {
            text: "visual observation".to_string(),
        },
        FunctionCallOutputContentItem::InputImage {
            image: ImageReference::Inline {
                image_url: "data:image/png;base64,AAAA".to_string(),
            },
            detail: None,
        },
    ];
    for (namespace, name) in [
        ("codex_browser", "browser_observe"),
        ("codex_browser", "browser_step"),
        ("codex_android", "android_observe"),
        ("codex_android", "android_step"),
        ("codex_android", "android_install_build_from_run"),
    ] {
        for success in [true, false] {
            let output = dynamic_tool_output(
                &ToolName::new(Some(namespace.to_string()), name),
                body.clone(),
                success,
            );
            assert_eq!(
                output.code_mode_result(&payload),
                json!({"content": body, "success": success})
            );
            let ResponseInputItem::FunctionCallOutput { output, .. } =
                output.to_response_item("native-call", &payload)
            else {
                panic!("native dynamic tools retain function output");
            };
            assert_eq!(
                output.body,
                FunctionCallOutputBody::ContentItems(body.clone())
            );
        }
    }
}

#[test]
fn other_dynamic_tools_keep_the_existing_code_mode_result_contract() {
    let payload = ToolPayload::Function {
        arguments: "{}".to_string(),
    };
    for tool_name in [
        ToolName::plain("browser_step"),
        ToolName::new(Some("other_namespace".to_string()), "browser_step"),
        ToolName::new(Some("codex_browser".to_string()), "unrecognized_tool"),
    ] {
        let output = dynamic_tool_output(
            &tool_name,
            vec![FunctionCallOutputContentItem::InputText {
                text: "ordinary result".to_string(),
            }],
            /*success*/ true,
        );
        assert_eq!(output.code_mode_result(&payload), json!("ordinary result"));
    }
}

async fn next_item(receiver: &async_channel::Receiver<Event>) -> DynamicToolCallItem {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let item = match receiver.recv().await.expect("event channel open").msg {
                EventMsg::ItemStarted(event) => event.item,
                EventMsg::ItemCompleted(event) => event.item,
                _ => continue,
            };
            if let TurnItem::DynamicToolCall(item) = item {
                return item;
            }
        }
    })
    .await
    .expect("dynamic tool item should be emitted")
}

#[tokio::test]
async fn response_cancel_race_completes_once() {
    let (session, turn, receiver) = crate::session::tests::make_session_and_context_with_rx().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());
    let cancellation = CancellationToken::new();
    let lifecycle = tokio::spawn({
        let session = Arc::clone(&session);
        let cancellation = cancellation.clone();
        async move {
            request_dynamic_tool(
                &session,
                &turn,
                "call-1".to_string(),
                ToolName::plain("gate"),
                json!({}),
                cancellation,
            )
            .await
        }
    });
    next_item(&receiver).await;
    let response = DynamicToolResponse {
        content_items: Vec::new(),
        success: true,
    };
    session
        .notify_dynamic_tool_response("call-1", response.clone())
        .await;
    cancellation.cancel();
    let result = lifecycle.await.expect("lifecycle joined");
    assert!(result.is_none() || result == Some(response));
    let item = next_item(&receiver).await;
    assert_eq!(
        (item.status, item.success, item.error),
        if result.is_some() {
            (DynamicToolCallStatus::Completed, Some(true), None)
        } else {
            (
                DynamicToolCallStatus::Failed,
                Some(false),
                Some("dynamic tool call was cancelled before receiving a response".to_string()),
            )
        }
    );
    while let Ok(event) = receiver.try_recv() {
        assert!(!matches!(event.msg, EventMsg::ItemCompleted(_)));
    }
}
