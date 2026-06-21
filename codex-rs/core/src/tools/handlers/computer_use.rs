use crate::function_tool::FunctionCallError;
use crate::original_image_detail::can_request_original_image_detail;
use crate::original_image_detail::sanitize_original_image_detail;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::hook_names::HookToolName;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::PreToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolExposure;
use codex_protocol::computer_use::ComputerUseCallRequest;
use codex_protocol::computer_use::ComputerUseOutputContentItem;
use codex_protocol::computer_use::ComputerUseResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::ComputerUseCallResponseEvent;
use codex_protocol::protocol::EventMsg;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSearchSourceInfo;
use codex_tools::ToolSpec;
use codex_tools::canonical_native_computer_use_dynamic_tool;
use serde_json::Value;
use serde_json::json;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tracing::warn;

pub struct ComputerUseHandler {
    tool_name: ToolName,
    adapter: String,
    spec: ToolSpec,
    exposure: ToolExposure,
    search_text: String,
    response_timeout: Duration,
}

pub struct ComputerUseOutput {
    tool_name: String,
    output: FunctionToolOutput,
}

const DEFAULT_COMPUTER_USE_TIMEOUT: Duration = Duration::from_secs(120);
const DEFAULT_COMPUTER_USE_INSTALL_TIMEOUT: Duration = Duration::from_secs(300);

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

impl ComputerUseHandler {
    pub fn from_dynamic_tool(tool: &DynamicToolSpec) -> Option<Self> {
        let native_tool = canonical_native_computer_use_dynamic_tool(tool)?;
        let response_timeout = if native_tool.uses_long_timeout {
            DEFAULT_COMPUTER_USE_INSTALL_TIMEOUT
        } else {
            DEFAULT_COMPUTER_USE_TIMEOUT
        };
        let output_tool = native_tool.tool;
        let search_text = [
            output_tool.name.clone(),
            output_tool.name.replace('_', " "),
            output_tool.description.clone(),
        ]
        .join(" ");
        let tool_name = ToolName::plain(output_tool.name.clone());
        Some(Self {
            tool_name,
            adapter: native_tool.adapter.to_string(),
            spec: ToolSpec::Function(output_tool),
            exposure: if tool.defer_loading {
                ToolExposure::Deferred
            } else {
                ToolExposure::Direct
            },
            search_text,
            response_timeout,
        })
    }

    pub fn planned_tool_name(&self) -> ToolName {
        self.tool_name.clone()
    }
}

impl ToolExecutor<ToolInvocation> for ComputerUseHandler {
    fn tool_name(&self) -> ToolName {
        self.tool_name.clone()
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn exposure(&self) -> ToolExposure {
        self.exposure
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        ToolSearchInfo::from_spec(
            self.search_text.clone(),
            self.spec(),
            Some(ToolSearchSourceInfo {
                name: "Native computer-use tools".to_string(),
                description: Some(
                    "Client-backed computer-use tools for the current environment.".to_string(),
                ),
            }),
        )
    }

    fn handle(&self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'_> {
        Box::pin(self.handle_call(invocation))
    }
}

impl ComputerUseHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
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
        let output_tool_name = tool_name.to_string();
        let response = request_computer_use(
            &session,
            turn.as_ref(),
            call_id,
            self.adapter.clone(),
            tool_name,
            args,
            self.response_timeout,
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
        Ok(boxed_tool_output(ComputerUseOutput {
            tool_name: output_tool_name,
            output: FunctionToolOutput::from_content(body, Some(success)),
        }))
    }
}

impl CoreToolRuntime for ComputerUseHandler {
    fn pre_tool_use_payload(&self, invocation: &ToolInvocation) -> Option<PreToolUsePayload> {
        let ToolPayload::Function { arguments } = &invocation.payload else {
            return None;
        };
        let tool_name = invocation.tool_name.to_string();
        Some(PreToolUsePayload {
            tool_name: HookToolName::new(tool_name.clone()),
            tool_input: json!({ "command": computer_use_command(&tool_name, arguments) }),
        })
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn ToolOutput,
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
    adapter: String,
    tool_name: ToolName,
    arguments: Value,
    response_timeout: Duration,
) -> Option<ComputerUseResponse> {
    let tool = tool_name.name;
    let turn_id = turn_context.sub_id.clone();
    let environment_id = selected_computer_use_environment_id(turn_context);
    let started_at = Instant::now();
    if environment_id.is_none() {
        let response = unavailable_response(&format!(
            "{} computer-use environment is unavailable: no turn environment is selected.",
            adapter_display_name(&adapter)
        ));
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
    turn_context
        .environments
        .turn_environments
        .first()
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

fn adapter_display_name(adapter: &str) -> String {
    match adapter {
        "android" => "Android".to_string(),
        "browser" => "Browser".to_string(),
        other => other.to_string(),
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
    use crate::session::tests::make_session_and_context_with_rx;
    use crate::session::turn_context::TurnEnvironment;
    use crate::state::ActiveTurn;
    use crate::tools::registry::ToolExecutor;
    use codex_protocol::computer_use::ComputerUseOutputContentItem;
    use codex_protocol::computer_use::ComputerUseResponse;
    use codex_protocol::dynamic_tools::DynamicToolSpec;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::protocol::EventMsg;
    use codex_tools::ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME;
    use codex_tools::ANDROID_OBSERVE_TOOL_NAME;
    use codex_tools::BROWSER_OBSERVE_TOOL_NAME;
    use codex_tools::COMPUTER_USE_ADAPTER_ANDROID;
    use codex_tools::COMPUTER_USE_ADAPTER_BROWSER;
    use codex_tools::LoadableToolSpec;
    use codex_tools::ToolName;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    fn native_dynamic_tool(name: &str, defer_loading: bool) -> DynamicToolSpec {
        DynamicToolSpec {
            namespace: None,
            name: name.to_string(),
            description: format!("{name} dynamic tool"),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            }),
            defer_loading,
            persist_on_resume: true,
            capability: None,
        }
    }

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

    #[test]
    fn browser_handler_uses_browser_adapter() {
        let observe_handler = ComputerUseHandler::from_dynamic_tool(&native_dynamic_tool(
            BROWSER_OBSERVE_TOOL_NAME,
            /*defer_loading*/ false,
        ))
        .expect("browser observe should create a native computer-use handler");

        assert_eq!(observe_handler.adapter, COMPUTER_USE_ADAPTER_BROWSER);
    }

    #[test]
    fn android_handler_uses_android_adapter() {
        let observe_handler = ComputerUseHandler::from_dynamic_tool(&native_dynamic_tool(
            ANDROID_OBSERVE_TOOL_NAME,
            /*defer_loading*/ false,
        ))
        .expect("android observe should create a native computer-use handler");
        let install_handler = ComputerUseHandler::from_dynamic_tool(&native_dynamic_tool(
            ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME,
            /*defer_loading*/ false,
        ))
        .expect("android install should create a native computer-use handler");

        assert_eq!(observe_handler.adapter, COMPUTER_USE_ADAPTER_ANDROID);
        assert_eq!(observe_handler.response_timeout, Duration::from_secs(120));
        assert_eq!(install_handler.adapter, COMPUTER_USE_ADAPTER_ANDROID);
        assert_eq!(install_handler.response_timeout, Duration::from_secs(300));
    }

    #[test]
    fn search_info_uses_native_computer_use_metadata() {
        let observe_handler = ComputerUseHandler::from_dynamic_tool(&native_dynamic_tool(
            ANDROID_OBSERVE_TOOL_NAME,
            /*defer_loading*/ true,
        ))
        .expect("android observe should create a native computer-use handler");

        let search_info = observe_handler
            .search_info()
            .expect("native computer-use search info");
        assert!(
            search_info
                .entry
                .search_text
                .contains(ANDROID_OBSERVE_TOOL_NAME),
            "search text should include the canonical native tool name: {}",
            search_info.entry.search_text
        );
        assert!(
            search_info.entry.search_text.contains("android observe"),
            "search text should include the native display name: {}",
            search_info.entry.search_text
        );
        assert!(
            search_info
                .entry
                .search_text
                .contains("Capture the current Android screen"),
            "search text should include the canonical native description: {}",
            search_info.entry.search_text
        );
        let source_info = search_info
            .source_info
            .expect("native computer-use source info");
        assert_eq!(source_info.name, "Native computer-use tools");
        assert_eq!(
            source_info.description.as_deref(),
            Some("Client-backed computer-use tools for the current environment.")
        );
        let LoadableToolSpec::Function(tool) = search_info.entry.output else {
            panic!("native computer-use search info should expose one function");
        };
        assert_eq!(tool.name, ANDROID_OBSERVE_TOOL_NAME);
        assert_eq!(tool.defer_loading, Some(true));
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

    #[test]
    fn native_image_response_reaches_model_content_items() {
        assert_eq!(
            computer_use_response_content_for_model(ComputerUseResponse {
                content_items: vec![
                    ComputerUseOutputContentItem::InputText {
                        text: "Android observation".to_string(),
                    },
                    ComputerUseOutputContentItem::InputImage {
                        image_url: "data:image/png;base64,AAAA".to_string(),
                        detail: Some("high".to_string()),
                    },
                ],
                success: true,
                error: None,
            }),
            (
                vec![
                    FunctionCallOutputContentItem::InputText {
                        text: "Android observation".to_string(),
                    },
                    FunctionCallOutputContentItem::InputImage {
                        image_url: "data:image/png;base64,AAAA".to_string(),
                        detail: Some(codex_protocol::models::ImageDetail::High),
                    },
                ],
                true,
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
        turn_context.environments = crate::environment_selection::TurnEnvironmentSnapshot {
            turn_environments: vec![
                TurnEnvironment::new(
                    "first".to_string(),
                    first_environment,
                    codex_utils_path_uri::PathUri::from_abs_path(&cwd),
                    None,
                ),
                TurnEnvironment::new(
                    "second".to_string(),
                    second_environment,
                    codex_utils_path_uri::PathUri::from_abs_path(&cwd),
                    None,
                ),
            ],
            starting: Vec::new(),
        };

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
            COMPUTER_USE_ADAPTER_ANDROID.to_string(),
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
            COMPUTER_USE_ADAPTER_ANDROID.to_string(),
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
