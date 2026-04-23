use crate::function_tool::FunctionCallError;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use codex_protocol::dynamic_tools::DynamicToolCallRequest;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::protocol::DynamicToolCallResponseEvent;
use codex_protocol::protocol::EventMsg;
use serde_json::Value;
use std::time::Instant;
use tokio::sync::oneshot;
use tracing::warn;

pub struct DynamicToolHandler;

const ANDROID_OBSERVE_TOOL_NAME: &str = "android_observe";

impl ToolHandler for DynamicToolHandler {
    type Output = FunctionToolOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        let tool_name = invocation.tool_name.display();
        match invocation
            .session
            .dynamic_tool_by_name(&tool_name)
            .await
            .and_then(|tool| tool.capability)
            .and_then(|capability| capability.mutation_class)
            .as_deref()
        {
            Some("read_only") => false,
            Some("mutating") => true,
            _ => tool_name != ANDROID_OBSERVE_TOOL_NAME,
        }
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        Some(PreToolUsePayload {
            command: dynamic_tool_command(&invocation.tool_name.display(), arguments),
        })
    }

    fn post_tool_use_payload(
        &self,
        _call_id: &str,
        payload: &ToolPayload,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        let ToolPayload::Function { arguments } = payload else {
            return None;
        };
        Some(PostToolUsePayload {
            command: dynamic_tool_command("brokered_tool", arguments),
            tool_response: result.code_mode_result(payload),
        })
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            call_id,
            tool_name,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "dynamic tool handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: Value = parse_arguments(&arguments)?;
        let response =
            request_dynamic_tool(&session, turn.as_ref(), call_id, tool_name.display(), args)
                .await
                .ok_or_else(|| {
                    FunctionCallError::RespondToModel(
                        "dynamic tool call was cancelled before receiving a response".to_string(),
                    )
                })?;

        let DynamicToolResponse {
            content_items,
            success,
        } = response;
        let body = content_items
            .into_iter()
            .map(FunctionCallOutputContentItem::from)
            .collect::<Vec<_>>();
        Ok(FunctionToolOutput::from_content(body, Some(success)))
    }
}

fn dynamic_tool_command(tool_name: &str, arguments: &str) -> String {
    match serde_json::from_str::<Value>(arguments) {
        Ok(arguments) => format!(
            "{tool_name} {}",
            serde_json::to_string(&arguments).unwrap_or_else(|_| arguments.to_string())
        ),
        Err(_) => format!("{tool_name} {arguments}"),
    }
}

async fn request_dynamic_tool(
    session: &Session,
    turn_context: &TurnContext,
    call_id: String,
    tool: String,
    arguments: Value,
) -> Option<DynamicToolResponse> {
    let turn_id = turn_context.sub_id.clone();
    let (tx_response, rx_response) = oneshot::channel();
    let event_id = call_id.clone();
    let prev_entry = {
        let mut active = session.active_turn.lock().await;
        match active.as_mut() {
            Some(at) => {
                let mut ts = at.turn_state.lock().await;
                ts.insert_pending_dynamic_tool(call_id.clone(), tx_response)
            }
            None => None,
        }
    };
    if prev_entry.is_some() {
        warn!("Overwriting existing pending dynamic tool call for call_id: {event_id}");
    }

    let started_at = Instant::now();
    let event = EventMsg::DynamicToolCallRequest(DynamicToolCallRequest {
        call_id: call_id.clone(),
        turn_id: turn_id.clone(),
        tool: tool.clone(),
        arguments: arguments.clone(),
    });
    session.send_event(turn_context, event).await;
    let response = rx_response.await.ok();

    let response_event = match &response {
        Some(response) => EventMsg::DynamicToolCallResponse(DynamicToolCallResponseEvent {
            call_id,
            turn_id,
            tool,
            arguments,
            content_items: response.content_items.clone(),
            success: response.success,
            error: None,
            duration: started_at.elapsed(),
        }),
        None => EventMsg::DynamicToolCallResponse(DynamicToolCallResponseEvent {
            call_id,
            turn_id,
            tool,
            arguments,
            content_items: Vec::new(),
            success: false,
            error: Some("dynamic tool call was cancelled before receiving a response".to_string()),
            duration: started_at.elapsed(),
        }),
    };
    session.send_event(turn_context, response_event).await;

    response
}

#[cfg(test)]
mod tests {
    use super::ANDROID_OBSERVE_TOOL_NAME;
    use super::DynamicToolHandler;
    use super::dynamic_tool_command;
    use crate::session::tests::make_session_and_context;
    use crate::session::tests::make_session_and_context_with_dynamic_tools_and_rx;
    use crate::tools::context::FunctionToolOutput;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::registry::ToolHandler;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::dynamic_tools::DynamicToolCapability;
    use codex_protocol::dynamic_tools::DynamicToolSpec;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[test]
    fn dynamic_tool_command_uses_compact_json_arguments() {
        assert_eq!(
            dynamic_tool_command(
                "android_observe",
                &json!({"scope": "screen_and_ui"}).to_string()
            ),
            r#"android_observe {"scope":"screen_and_ui"}"#
        );
    }

    #[tokio::test]
    async fn android_observe_is_non_mutating() {
        let (session, turn) = make_session_and_context().await;
        let handler = DynamicToolHandler;
        let payload = ToolPayload::Function {
            arguments: json!({"scope": "screen_and_ui"}).to_string(),
        };

        assert!(
            !handler
                .is_mutating(&ToolInvocation {
                    session: session.into(),
                    turn: turn.into(),
                    tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                    call_id: "call-1".to_string(),
                    tool_name: codex_tools::ToolName::plain(ANDROID_OBSERVE_TOOL_NAME),
                    payload,
                })
                .await
        );
    }

    #[tokio::test]
    async fn dynamic_tool_mutation_uses_capability_metadata_when_present() {
        let (session, turn, _rx) =
            make_session_and_context_with_dynamic_tools_and_rx(vec![DynamicToolSpec {
                name: "brokered_read".to_string(),
                description: "read from an environment-bound capability".to_string(),
                input_schema: json!({"type": "object", "properties": {}}),
                defer_loading: false,
                persist_on_resume: false,
                capability: Some(DynamicToolCapability {
                    family: Some("android".to_string()),
                    capability_scope: Some("environment".to_string()),
                    mutation_class: Some("read_only".to_string()),
                    lease_mode: Some("shared_read".to_string()),
                }),
            }])
            .await;
        let handler = DynamicToolHandler;
        let payload = ToolPayload::Function {
            arguments: json!({"scope": "screen"}).to_string(),
        };

        assert!(
            !handler
                .is_mutating(&ToolInvocation {
                    session: session.into(),
                    turn: turn.into(),
                    tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                    call_id: "call-3".to_string(),
                    tool_name: codex_tools::ToolName::plain("brokered_read"),
                    payload,
                })
                .await
        );
    }

    #[tokio::test]
    async fn dynamic_tool_pre_and_post_payloads_preserve_arguments() {
        let (session, turn) = make_session_and_context().await;
        let payload = ToolPayload::Function {
            arguments: json!({"scope": "screen_and_ui"}).to_string(),
        };
        let invocation = ToolInvocation {
            session: session.into(),
            turn: turn.into(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-2".to_string(),
            tool_name: codex_tools::ToolName::plain(ANDROID_OBSERVE_TOOL_NAME),
            payload: payload.clone(),
        };
        let handler = DynamicToolHandler;
        let output = FunctionToolOutput {
            body: vec![FunctionCallOutputContentItem::InputText {
                text: "screen summary".to_string(),
            }],
            success: Some(true),
            post_tool_use_response: Some(json!({"ok": true})),
        };

        assert_eq!(
            handler.pre_tool_use_payload(&invocation),
            Some(crate::tools::registry::PreToolUsePayload {
                command: r#"android_observe {"scope":"screen_and_ui"}"#.to_string(),
            })
        );
        assert_eq!(
            handler.post_tool_use_payload("call-2", &payload, &output),
            Some(crate::tools::registry::PostToolUsePayload {
                command: r#"brokered_tool {"scope":"screen_and_ui"}"#.to_string(),
                tool_response: json!("screen summary"),
            })
        );
    }
}
