use crate::function_tool::FunctionCallError;
use crate::original_image_detail::can_request_original_image_detail;
use crate::original_image_detail::sanitize_original_image_detail;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use codex_protocol::dynamic_tools::DynamicToolCallRequest;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::DynamicToolCallResponseEvent;
use codex_protocol::protocol::EventMsg;
use codex_tools::ToolName;
use serde_json::Value;
use serde_json::json;
use std::time::Instant;
use tokio::sync::oneshot;
use tracing::warn;

pub struct DynamicToolHandler;

pub struct DynamicToolOutput {
    tool_name: String,
    output: FunctionToolOutput,
}

const ANDROID_OBSERVE_TOOL_NAME: &str = "android_observe";

impl ToolOutput for DynamicToolOutput {
    fn log_preview(&self) -> String {
        self.output.log_preview()
    }

    fn success_for_logging(&self) -> bool {
        self.output.success_for_logging()
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.output.to_response_item(call_id, payload)
    }

    fn post_tool_use_response(&self, call_id: &str, payload: &ToolPayload) -> Option<Value> {
        let tool_response = self
            .output
            .post_tool_use_response(call_id, payload)
            .unwrap_or_else(|| self.output.code_mode_result(payload));
        Some(json!({
            "tool_name": self.tool_name.as_str(),
            "tool_response": tool_response,
        }))
    }

    fn code_mode_result(&self, payload: &ToolPayload) -> Value {
        self.output.code_mode_result(payload)
    }
}

impl ToolHandler for DynamicToolHandler {
    type Output = DynamicToolOutput;

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
        let tool_name = invocation.tool_name.display();
        Some(PreToolUsePayload {
            tool_name: HookToolName::new(tool_name.clone()),
            tool_input: json!({ "command": dynamic_tool_command(&tool_name, arguments) }),
        })
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &Self::Output,
    ) -> Option<PostToolUsePayload> {
        let call_id = invocation.call_id.as_str();
        let payload = &invocation.payload;
        let ToolPayload::Function { arguments } = payload else {
            return None;
        };

        let fallback_tool_name = "brokered_tool";
        let fallback_tool_input =
            json!({ "command": dynamic_tool_command(fallback_tool_name, arguments) });
        match result.post_tool_use_response(call_id, payload) {
            Some(tool_response) => match tool_response
                .as_object()
                .and_then(|response| response.get("tool_name").and_then(Value::as_str))
            {
                Some(tool_name) => {
                    let tool_name = tool_name.to_owned();
                    let tool_response = tool_response
                        .as_object()
                        .and_then(|response| response.get("tool_response"))
                        .cloned()
                        .unwrap_or_else(|| tool_response.clone());
                    Some(PostToolUsePayload {
                        tool_name: HookToolName::new(tool_name.clone()),
                        tool_use_id: call_id.to_string(),
                        tool_input: json!({
                            "command": dynamic_tool_command(&tool_name, arguments)
                        }),
                        tool_response,
                    })
                }
                None => Some(PostToolUsePayload {
                    tool_name: HookToolName::new(fallback_tool_name),
                    tool_use_id: call_id.to_string(),
                    tool_input: fallback_tool_input,
                    tool_response,
                }),
            },
            None => Some(PostToolUsePayload {
                tool_name: HookToolName::new(fallback_tool_name),
                tool_use_id: call_id.to_string(),
                tool_input: fallback_tool_input,
                tool_response: result.code_mode_result(payload),
            }),
        }
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
        let output_tool_name = tool_name.display();
        let response = request_dynamic_tool(&session, turn.as_ref(), call_id, tool_name, args)
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
        let mut body = content_items
            .into_iter()
            .map(FunctionCallOutputContentItem::from)
            .collect::<Vec<_>>();
        sanitize_original_image_detail(
            can_request_original_image_detail(&turn.model_info),
            &mut body,
        );
        Ok(DynamicToolOutput {
            tool_name: output_tool_name,
            output: FunctionToolOutput::from_content(body, Some(success)),
        })
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

#[expect(
    clippy::await_holding_invalid_type,
    reason = "active turn checks and dynamic tool response registration must remain atomic"
)]
async fn request_dynamic_tool(
    session: &Session,
    turn_context: &TurnContext,
    call_id: String,
    tool_name: ToolName,
    arguments: Value,
) -> Option<DynamicToolResponse> {
    let namespace = tool_name.namespace;
    let tool = tool_name.name;
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
        namespace: namespace.clone(),
        tool: tool.clone(),
        arguments: arguments.clone(),
    });
    session.send_event(turn_context, event).await;
    let response = rx_response.await.ok();

    let response_event = match &response {
        Some(response) => EventMsg::DynamicToolCallResponse(DynamicToolCallResponseEvent {
            call_id,
            turn_id,
            namespace,
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
            namespace,
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
    use super::DynamicToolOutput;
    use super::dynamic_tool_command;
    use crate::session::tests::make_session_and_context;
    use crate::session::tests::make_session_and_context_with_dynamic_tools_and_rx;
    use crate::tools::context::FunctionToolOutput;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolOutput;
    use crate::tools::context::ToolPayload;
    use crate::tools::hook_names::HookToolName;
    use crate::tools::registry::ToolHandler;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::dynamic_tools::DynamicToolCapability;
    use codex_protocol::dynamic_tools::DynamicToolSpec;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

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
                    cancellation_token: CancellationToken::new(),
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
                namespace: None,
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
                    cancellation_token: CancellationToken::new(),
                    tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                    call_id: "call-3".to_string(),
                    tool_name: codex_tools::ToolName::plain("brokered_read"),
                    payload,
                })
                .await
        );
    }

    #[tokio::test]
    async fn dynamic_tool_pre_and_post_payloads_use_real_tool_name_and_custom_post_response() {
        let (session, turn) = make_session_and_context().await;
        let payload = ToolPayload::Function {
            arguments: json!({"scope": "screen_and_ui"}).to_string(),
        };
        let invocation = ToolInvocation {
            session: session.into(),
            turn: turn.into(),
            cancellation_token: CancellationToken::new(),
            tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
            call_id: "call-2".to_string(),
            tool_name: codex_tools::ToolName::plain(ANDROID_OBSERVE_TOOL_NAME),
            payload: payload.clone(),
        };
        let handler = DynamicToolHandler;
        let output = DynamicToolOutput {
            tool_name: ANDROID_OBSERVE_TOOL_NAME.to_string(),
            output: FunctionToolOutput {
                body: vec![FunctionCallOutputContentItem::InputText {
                    text: "screen summary".to_string(),
                }],
                success: Some(true),
                post_tool_use_response: Some(json!({"ok": true})),
            },
        };

        assert_eq!(
            handler.pre_tool_use_payload(&invocation),
            Some(crate::tools::registry::PreToolUsePayload {
                tool_name: HookToolName::new(ANDROID_OBSERVE_TOOL_NAME),
                tool_input: json!({
                    "command": r#"android_observe {"scope":"screen_and_ui"}"#,
                }),
            })
        );
        assert_eq!(
            output.post_tool_use_response("call-2", &payload),
            Some(json!({
                "tool_name": ANDROID_OBSERVE_TOOL_NAME,
                "tool_response": {"ok": true},
            }))
        );
        assert_eq!(
            handler.post_tool_use_payload(&invocation, &output),
            Some(crate::tools::registry::PostToolUsePayload {
                tool_name: HookToolName::new(ANDROID_OBSERVE_TOOL_NAME),
                tool_use_id: "call-2".to_string(),
                tool_input: json!({
                    "command": r#"android_observe {"scope":"screen_and_ui"}"#,
                }),
                tool_response: json!({"ok": true}),
            })
        );
    }
}
