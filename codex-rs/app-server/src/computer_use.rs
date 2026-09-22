use codex_app_server_protocol::ComputerUseCallParams;
use codex_app_server_protocol::ComputerUseCallResponse;
use std::future::Future;

/// Routes one typed computer-use request to an injected provider.
///
/// Keeping the provider as a callback makes the app-server seam mockable while
/// leaving provider discovery and implementation to the owning package.
pub(crate) async fn route_call<F, Fut>(
    params: ComputerUseCallParams,
    provider: F,
) -> ComputerUseCallResponse
where
    F: FnOnce(ComputerUseCallParams) -> Fut,
    Fut: Future<Output = ComputerUseCallResponse>,
{
    provider(params).await
}

#[cfg(test)]
mod tests {
    use super::route_call;
    use codex_app_server_protocol::ComputerUseCallOutputContentItem;
    use codex_app_server_protocol::ComputerUseCallParams;
    use codex_app_server_protocol::ComputerUseCallResponse;
    use serde_json::json;

    #[tokio::test]
    async fn routes_typed_request_to_mock_provider_and_returns_typed_response() {
        let params = ComputerUseCallParams {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "call".to_string(),
            environment_id: Some("android".to_string()),
            adapter: "mock".to_string(),
            tool: "tap".to_string(),
            arguments: json!({"x": 10, "y": 20}),
        };

        let response = route_call(params, |request| async move {
            assert_eq!(request.adapter, "mock");
            assert_eq!(request.tool, "tap");
            ComputerUseCallResponse {
                content_items: vec![ComputerUseCallOutputContentItem::InputText {
                    text: "ok".to_string(),
                }],
                success: true,
            }
        })
        .await;

        assert!(response.success);
        assert!(matches!(
            response.content_items.as_slice(),
            [ComputerUseCallOutputContentItem::InputText { text }] if text == "ok"
        ));
    }
}
