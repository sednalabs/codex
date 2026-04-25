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
use codex_protocol::computer_use::ComputerUseCallRequest;
use codex_protocol::computer_use::ComputerUseOutputContentItem;
use codex_protocol::computer_use::ComputerUseResponse;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::ComputerUseCallResponseEvent;
use codex_protocol::protocol::EventMsg;
use codex_tools::ANDROID_OBSERVE_TOOL_NAME;
use codex_tools::ToolName;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tracing::warn;

pub struct ComputerUseHandler;

pub struct ComputerUseOutput {
    tool_name: String,
    output: FunctionToolOutput,
}

const ADAPTER_ANDROID: &str = "android";
const DEFAULT_COMPUTER_USE_TIMEOUT: Duration = Duration::from_secs(120);

impl ToolOutput for ComputerUseOutput {
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

impl ToolHandler for ComputerUseHandler {
    type Output = ComputerUseOutput;

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    async fn is_mutating(&self, invocation: &ToolInvocation) -> bool {
        invocation.tool_name.name != ANDROID_OBSERVE_TOOL_NAME
    }

    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        let tool_name = invocation.tool_name.display();
        Some(PreToolUsePayload {
            tool_name: HookToolName::new(tool_name.clone()),
            tool_input: json!({ "command": computer_use_command(&tool_name, arguments) }),
        })
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &Self::Output,
    ) -> Option<PostToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };

        let call_id = invocation.call_id.as_str();
        let payload = &invocation.payload;
        let fallback_tool_name = "computer_use";
        let fallback_tool_input =
            json!({ "command": computer_use_command(fallback_tool_name, arguments) });
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
                            "command": computer_use_command(&tool_name, arguments)
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
                    "computer-use handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: Value = parse_arguments(&arguments)?;
        let output_tool_name = tool_name.display();
        let response = request_computer_use(
            &session,
            turn.as_ref(),
            call_id,
            tool_name,
            args,
            DEFAULT_COMPUTER_USE_TIMEOUT,
        )
        .await
        .ok_or_else(|| {
            FunctionCallError::RespondToModel(
                "computer-use call was cancelled before receiving a response".to_string(),
            )
        })?;

        let (mut body, success) = computer_use_response_content_for_model(response);
        sanitize_original_image_detail(
            can_request_original_image_detail(&turn.model_info),
            &mut body,
        );
        Ok(ComputerUseOutput {
            tool_name: output_tool_name,
            output: FunctionToolOutput::from_content(body, Some(success)),
        })
    }
}

fn computer_use_response_content_for_model(
    response: ComputerUseResponse,
) -> (Vec<FunctionCallOutputContentItem>, bool) {
    let ComputerUseResponse {
        mut content_items,
        success,
        error,
    } = response;
    if !success
        && content_items.is_empty()
        && let Some(error) = error
    {
        content_items.push(ComputerUseOutputContentItem::InputText { text: error });
    }
    (
        content_items
            .into_iter()
            .map(FunctionCallOutputContentItem::from)
            .collect(),
        success,
    )
}

fn computer_use_command(tool_name: &str, arguments: &str) -> String {
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
    reason = "active turn checks and computer-use response registration must remain atomic"
)]
async fn request_computer_use(
    session: &Session,
    turn_context: &TurnContext,
    call_id: String,
    tool_name: ToolName,
    arguments: Value,
    response_timeout: Duration,
) -> Option<ComputerUseResponse> {
    let tool = tool_name.name;
    let turn_id = turn_context.sub_id.clone();
    let environment_id = selected_computer_use_environment_id(turn_context);
    let adapter = ADAPTER_ANDROID.to_string();
    let started_at = Instant::now();
    if environment_id.is_none() {
        let response = unavailable_response(
            "Android computer-use environment is unavailable: no turn environment is selected.",
        );
        session
            .send_event(
                turn_context,
                EventMsg::ComputerUseCallResponse(ComputerUseCallResponseEvent {
                    call_id,
                    turn_id,
                    environment_id,
                    adapter,
                    tool,
                    arguments,
                    content_items: response.content_items.clone(),
                    success: response.success,
                    error: response.error.clone(),
                    duration: started_at.elapsed(),
                }),
            )
            .await;
        return Some(response);
    }

    let request = ComputerUseCallRequest {
        call_id: call_id.clone(),
        turn_id: turn_id.clone(),
        environment_id: environment_id.clone(),
        adapter: adapter.clone(),
        tool: tool.clone(),
        arguments: arguments.clone(),
    };

    let pending_response = {
        let (tx_response, rx_response) = oneshot::channel();
        let prev_entry = {
            let mut active = session.active_turn.lock().await;
            match active.as_mut() {
                Some(at) => {
                    let mut ts = at.turn_state.lock().await;
                    ts.insert_pending_computer_use(call_id.clone(), tx_response)
                }
                None => None,
            }
        };
        if prev_entry.is_some() {
            warn!("Overwriting existing pending computer-use call for call_id: {call_id}");
        }
        rx_response
    };

    session
        .send_event(turn_context, EventMsg::ComputerUseCallRequest(request))
        .await;

    let response = match timeout(response_timeout, pending_response).await {
        Ok(Ok(response)) => Some(response),
        Ok(Err(_)) => None,
        Err(_) => {
            let mut active = session.active_turn.lock().await;
            if let Some(at) = active.as_mut() {
                let mut ts = at.turn_state.lock().await;
                ts.remove_pending_computer_use(&call_id);
            }
            let message = format!(
                "computer-use call timed out after {} ms waiting for a client response",
                response_timeout.as_millis()
            );
            Some(unavailable_response(&message))
        }
    };

    let response_event = match &response {
        Some(response) => EventMsg::ComputerUseCallResponse(ComputerUseCallResponseEvent {
            call_id,
            turn_id,
            environment_id,
            adapter,
            tool,
            arguments,
            content_items: response.content_items.clone(),
            success: response.success,
            error: response.error.clone(),
            duration: started_at.elapsed(),
        }),
        None => EventMsg::ComputerUseCallResponse(ComputerUseCallResponseEvent {
            call_id,
            turn_id,
            environment_id,
            adapter,
            tool,
            arguments,
            content_items: Vec::new(),
            success: false,
            error: Some("computer-use call was cancelled before receiving a response".to_string()),
            duration: started_at.elapsed(),
        }),
    };
    session.send_event(turn_context, response_event).await;

    response
}

fn selected_computer_use_environment_id(turn_context: &TurnContext) -> Option<String> {
    let primary_environment = turn_context.environment.as_ref()?;
    turn_context
        .environments
        .iter()
        .find(|environment| Arc::ptr_eq(&environment.environment, primary_environment))
        .map(|environment| environment.environment_id.clone())
}

fn unavailable_response(message: &str) -> ComputerUseResponse {
    ComputerUseResponse {
        content_items: vec![ComputerUseOutputContentItem::InputText {
            text: message.to_string(),
        }],
        success: false,
        error: Some(message.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::ComputerUseHandler;
    use super::computer_use_command;
    use super::computer_use_response_content_for_model;
    use super::request_computer_use;
    use super::selected_computer_use_environment_id;
    use super::unavailable_response;
    use crate::session::tests::make_session_and_context;
    use crate::session::tests::make_session_and_context_with_rx;
    use crate::session::turn_context::TurnEnvironment;
    use crate::state::ActiveTurn;
    use crate::tools::context::ToolCallSource;
    use crate::tools::context::ToolInvocation;
    use crate::tools::context::ToolPayload;
    use crate::tools::registry::ToolHandler;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_protocol::computer_use::ComputerUseOutputContentItem;
    use codex_protocol::computer_use::ComputerUseResponse;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::protocol::EventMsg;
    use codex_tools::ANDROID_OBSERVE_TOOL_NAME;
    use codex_tools::ANDROID_STEP_TOOL_NAME;
    use codex_tools::ToolName;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn computer_use_command_uses_compact_json_arguments() {
        assert_eq!(
            computer_use_command(
                "android_observe",
                &json!({"scope": "screen_and_ui"}).to_string()
            ),
            r#"android_observe {"scope":"screen_and_ui"}"#
        );
    }

    #[tokio::test]
    async fn android_observe_is_non_mutating_but_step_is_mutating() {
        let (session, turn) = make_session_and_context().await;
        let session = Arc::new(session);
        let turn = Arc::new(turn);
        let handler = ComputerUseHandler;

        let observe_payload = ToolPayload::Function {
            arguments: json!({"scope": "screen_and_ui"}).to_string(),
        };
        assert!(
            !handler
                .is_mutating(&ToolInvocation {
                    session: session.clone(),
                    turn: turn.clone(),
                    cancellation_token: CancellationToken::new(),
                    tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                    call_id: "call-observe".to_string(),
                    tool_name: codex_tools::ToolName::plain(ANDROID_OBSERVE_TOOL_NAME),
                    source: ToolCallSource::Direct,
                    payload: observe_payload,
                })
                .await
        );

        let step_payload = ToolPayload::Function {
            arguments: json!({"action": "tap", "x": 1, "y": 2}).to_string(),
        };
        assert!(
            handler
                .is_mutating(&ToolInvocation {
                    session,
                    turn,
                    cancellation_token: CancellationToken::new(),
                    tracker: Arc::new(Mutex::new(TurnDiffTracker::new())),
                    call_id: "call-step".to_string(),
                    tool_name: codex_tools::ToolName::plain(ANDROID_STEP_TOOL_NAME),
                    source: ToolCallSource::Direct,
                    payload: step_payload,
                })
                .await
        );
    }

    #[test]
    fn unavailable_response_uses_native_computer_use_content_items() {
        assert_eq!(
            unavailable_response("no android environment"),
            codex_protocol::computer_use::ComputerUseResponse {
                content_items: vec![ComputerUseOutputContentItem::InputText {
                    text: "no android environment".to_string(),
                }],
                success: false,
                error: Some("no android environment".to_string()),
            }
        );
    }

    #[test]
    fn failed_empty_response_returns_error_text_to_model() {
        assert_eq!(
            computer_use_response_content_for_model(ComputerUseResponse {
                content_items: Vec::new(),
                success: false,
                error: Some("android session disconnected".to_string()),
            }),
            (
                vec![FunctionCallOutputContentItem::InputText {
                    text: "android session disconnected".to_string(),
                }],
                false,
            )
        );
    }

    #[tokio::test]
    async fn selected_computer_use_environment_uses_primary_environment() {
        let (_session, turn_context, _rx) = make_session_and_context_with_rx().await;
        let mut turn_context =
            Arc::into_inner(turn_context).expect("turn context should have one owner");
        let cwd = turn_context.cwd.clone();
        let first_environment = Arc::new(
            codex_exec_server::Environment::create_for_tests(/*exec_server_url*/ None)
                .expect("create first environment"),
        );
        let second_environment = Arc::new(
            codex_exec_server::Environment::create_for_tests(/*exec_server_url*/ None)
                .expect("create second environment"),
        );
        turn_context.environment = Some(Arc::clone(&second_environment));
        turn_context.environments = vec![
            TurnEnvironment {
                environment_id: "first".to_string(),
                environment: first_environment,
                cwd: cwd.clone(),
            },
            TurnEnvironment {
                environment_id: "second".to_string(),
                environment: second_environment,
                cwd,
            },
        ];

        assert_eq!(
            selected_computer_use_environment_id(&turn_context),
            Some("second".to_string())
        );
    }

    #[tokio::test]
    async fn unavailable_environment_does_not_emit_external_computer_use_request() {
        let (session, turn, rx) = make_session_and_context_with_rx().await;

        let response = request_computer_use(
            &session,
            &turn,
            "call-no-env".to_string(),
            ToolName::plain(ANDROID_OBSERVE_TOOL_NAME),
            json!({ "scope": "screen_and_ui" }),
            Duration::from_secs(1),
        )
        .await
        .expect("no-environment calls should return a local response");

        assert!(!response.success);
        let event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("computer-use response event should be emitted")
            .expect("event channel should be open");
        let response_event = match event.msg {
            EventMsg::ComputerUseCallResponse(response_event) => response_event,
            other => panic!("expected computer-use response event, got {other:?}"),
        };
        assert_eq!(response_event.call_id, "call-no-env");
        assert!(response_event.environment_id.is_none());
        assert!(!response_event.success);
        assert_eq!(
            response_event.error.as_deref(),
            Some(
                "Android computer-use environment is unavailable: no turn environment is selected."
            )
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "no external ComputerUseCallRequest should be emitted"
        );
    }

    #[tokio::test]
    async fn computer_use_call_times_out_and_unregisters_pending_response() {
        let (session, turn, rx) = make_session_and_context_with_rx().await;
        *session.active_turn.lock().await = Some(ActiveTurn::default());

        let response = request_computer_use(
            &session,
            &turn,
            "call-timeout".to_string(),
            ToolName::plain(ANDROID_OBSERVE_TOOL_NAME),
            json!({ "scope": "screen_and_ui" }),
            Duration::from_millis(1),
        )
        .await
        .expect("timeout should return a structured failure response");

        assert_eq!(
            response,
            ComputerUseResponse {
                content_items: vec![ComputerUseOutputContentItem::InputText {
                    text: "computer-use call timed out after 1 ms waiting for a client response"
                        .to_string(),
                }],
                success: false,
                error: Some(
                    "computer-use call timed out after 1 ms waiting for a client response"
                        .to_string(),
                ),
            }
        );

        let request_event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("computer-use request event should be emitted")
            .expect("event channel should be open");
        assert!(matches!(
            request_event.msg,
            EventMsg::ComputerUseCallRequest(request) if request.call_id == "call-timeout"
        ));

        let response_event = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("computer-use timeout response event should be emitted")
            .expect("event channel should be open");
        let response_event = match response_event.msg {
            EventMsg::ComputerUseCallResponse(response_event) => response_event,
            other => panic!("expected computer-use response event, got {other:?}"),
        };
        assert_eq!(response_event.call_id, "call-timeout");
        assert!(!response_event.success);
        assert_eq!(response_event.error, response.error);

        session
            .notify_computer_use_response(
                "call-timeout",
                ComputerUseResponse {
                    content_items: Vec::new(),
                    success: true,
                    error: None,
                },
            )
            .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(50), rx.recv())
                .await
                .is_err(),
            "late client responses after timeout should not emit duplicate events"
        );
    }
}
