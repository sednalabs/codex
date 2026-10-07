use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use serde_json::Value;
use std::path::Path;

pub(crate) const NAMESPACE: &str = "codex_browser";

pub(crate) fn is_browser_call(params: &DynamicToolCallParams) -> bool {
    params.namespace.as_deref() == Some(NAMESPACE)
}

pub(crate) fn specs_for_codex_home(codex_home: &Path) -> Vec<DynamicToolSpec> {
    codex_browser_computer_use::configured_browser_dynamic_tools_for_codex_home(codex_home)
}

pub(crate) async fn handle_for_codex_home(
    params: &DynamicToolCallParams,
    codex_home: &Path,
) -> Option<DynamicToolCallResponse> {
    match codex_browser_computer_use::handle_browser_computer_use_for_codex_home(params, codex_home)
        .await
    {
        codex_browser_computer_use::BrowserComputerUseOutcome::Unavailable => None,
        codex_browser_computer_use::BrowserComputerUseOutcome::Handled(response) => Some(response),
    }
}

pub(crate) fn completed_event(
    request_id: RequestId,
    response: DynamicToolCallResponse,
) -> crate::app_event::AppEvent {
    crate::app_event::AppEvent::DynamicToolCallCompleted {
        request_id,
        response,
    }
}

pub(crate) fn validate_no_reserved_name_conflict(params: &ThreadStartParams) -> Result<(), String> {
    if let Some(specs) = &params.dynamic_tools {
        for spec in specs {
            match spec {
                DynamicToolSpec::Namespace(namespace) if namespace.name == NAMESPACE => {
                    return Err(format!(
                        "Browser dynamic namespace `{NAMESPACE}` is reserved; remove the configured namespace before starting this thread."
                    ));
                }
                DynamicToolSpec::Function(function) if function.name == NAMESPACE => {
                    return Err(format!(
                        "Browser dynamic function `{NAMESPACE}` is reserved; remove the configured function before starting this thread."
                    ));
                }
                _ => {}
            }
        }
    }
    if let Some(config) = &params.config {
        if config.contains_key(&format!("mcp_servers.{NAMESPACE}"))
            || config
                .get("mcp_servers")
                .and_then(Value::as_object)
                .is_some_and(|servers| servers.contains_key(NAMESPACE))
        {
            return Err(format!(
                "MCP server key `{NAMESPACE}` is reserved by Browser dynamic tools; rename that server before starting this thread."
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::DynamicToolCallOutputContentItem;
    use codex_app_server_protocol::DynamicToolFunctionSpec;
    use codex_app_server_protocol::DynamicToolNamespaceSpec;
    use codex_app_server_protocol::DynamicToolNamespaceTool;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn unrelated_namespace_can_reuse_local_browser_tool_names() {
        let mut params = ThreadStartParams::default();
        params.dynamic_tools = Some(vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
            name: "other_namespace".to_string(),
            description: String::new(),
            tools: vec![DynamicToolNamespaceTool::Function(
                DynamicToolFunctionSpec {
                    name: "browser_step".to_string(),
                    description: String::new(),
                    input_schema: json!({"type":"object"}),
                    defer_loading: false,
                },
            )],
        })]);
        assert!(validate_no_reserved_name_conflict(&params).is_ok());
    }

    #[test]
    fn reserved_collisions_are_rejected_without_mutating_inputs() {
        let mut params = ThreadStartParams::default();
        params.dynamic_tools = Some(vec![DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
            name: NAMESPACE.to_string(),
            description: String::new(),
            tools: Vec::new(),
        })]);
        let before = params.dynamic_tools.clone();
        assert!(validate_no_reserved_name_conflict(&params).is_err());
        assert_eq!(params.dynamic_tools, before);

        params.dynamic_tools = None;
        params.config = Some(HashMap::from([(
            "mcp_servers".to_string(),
            json!({(NAMESPACE): {"url":"http://example.test"}}),
        )]));
        let before = params.config.clone();
        assert!(validate_no_reserved_name_conflict(&params).is_err());
        assert_eq!(params.config, before);
    }

    #[test]
    fn browser_completion_keeps_request_correlation_and_typed_image() {
        let event = completed_event(
            RequestId::String("browser-request".to_string()),
            DynamicToolCallResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAAA".to_string(),
                }],
                success: true,
            },
        );
        let crate::app_event::AppEvent::DynamicToolCallCompleted {
            request_id: RequestId::String(request_id),
            response,
        } = event
        else {
            panic!("expected correlated Browser dynamic-tool completion");
        };
        assert_eq!(request_id, "browser-request");
        assert!(matches!(
            response.content_items.as_slice(),
            [DynamicToolCallOutputContentItem::InputImage { image_url }]
                if image_url == "data:image/png;base64,AAAA"
        ));
    }
}
