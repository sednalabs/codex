use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use serde_json::Value;
use std::path::Path;

pub(crate) const NAMESPACE: &str = "codex_android";

pub(crate) fn is_android_call(params: &DynamicToolCallParams) -> bool {
    params.namespace.as_deref() == Some(NAMESPACE)
}

pub(crate) fn specs_for_codex_home(codex_home: &Path) -> Vec<DynamicToolSpec> {
    codex_android_computer_use::configured_android_dynamic_tools_for_codex_home(codex_home)
}

pub(crate) async fn handle_for_codex_home(
    params: &DynamicToolCallParams,
    codex_home: &Path,
) -> Option<DynamicToolCallResponse> {
    match codex_android_computer_use::handle_android_computer_use_for_codex_home(params, codex_home)
        .await
    {
        codex_android_computer_use::AndroidComputerUseOutcome::Unavailable => None,
        codex_android_computer_use::AndroidComputerUseOutcome::Handled(response) => Some(response),
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
                        "Android dynamic namespace `{NAMESPACE}` is reserved; remove the configured namespace before starting this thread."
                    ));
                }
                DynamicToolSpec::Function(function) if function.name == NAMESPACE => {
                    return Err(format!(
                        "Android dynamic function `{NAMESPACE}` is reserved; remove the configured function before starting this thread."
                    ));
                }
                _ => {}
            }
        }
    }
    if let Some(config) = &params.config
        && (config.contains_key(&format!("mcp_servers.{NAMESPACE}"))
            || config
                .get("mcp_servers")
                .and_then(Value::as_object)
                .is_some_and(|servers| servers.contains_key(NAMESPACE)))
    {
        return Err(format!(
            "MCP server key `{NAMESPACE}` is reserved by Android dynamic tools; rename that server before starting this thread."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_app_server_protocol::DynamicToolCallOutputContentItem;

    #[test]
    fn completion_keeps_request_correlation_and_native_image() {
        let event = completed_event(
            RequestId::String("android-turn-call".to_string()),
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
            panic!("expected correlated Android dynamic-tool completion");
        };
        assert_eq!(request_id, "android-turn-call");
        assert!(response.success);
        assert!(matches!(
            response.content_items.as_slice(),
            [DynamicToolCallOutputContentItem::InputImage { image_url }]
                if image_url == "data:image/png;base64,AAAA"
        ));
    }
}
