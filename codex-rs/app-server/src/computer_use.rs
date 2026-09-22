//! The app-server boundary for native computer-use calls.
//!
//! The provider implementation lives outside app-server. This small contract keeps
//! routing typed and makes the request/response seam independently testable.

use codex_app_server_protocol::ComputerUseCallParams;
use codex_app_server_protocol::ComputerUseCallResponse;
use std::future::Future;
use std::pin::Pin;

pub(crate) type ComputerUseFuture =
    Pin<Box<dyn Future<Output = ComputerUseCallResponse> + Send + 'static>>;

/// Provider-owned implementation of a native computer-use call.
pub(crate) trait ComputerUseProvider: Send + Sync + 'static {
    fn call(&self, request: ComputerUseCallParams) -> ComputerUseFuture;
}

/// Route one typed app-server request to the provider and return its typed response.
pub(crate) async fn route_call<P>(provider: &P, request: ComputerUseCallParams) -> ComputerUseCallResponse
where
    P: ComputerUseProvider,
{
    provider.call(request).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct MockProvider;

    impl ComputerUseProvider for MockProvider {
        fn call(&self, request: ComputerUseCallParams) -> ComputerUseFuture {
            Box::pin(async move {
                ComputerUseCallResponse {
                    content_items: vec![
                        codex_app_server_protocol::ComputerUseCallOutputContentItem::InputText {
                            text: format!("{}:{}", request.adapter, request.tool),
                        },
                    ],
                    success: request.arguments == json!({"ok": true}),
                }
            })
        }
    }

    #[tokio::test]
    async fn typed_request_routes_to_mock_provider() {
        let request = ComputerUseCallParams {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "call".to_string(),
            environment_id: None,
            adapter: "mock".to_string(),
            tool: "click".to_string(),
            arguments: json!({"ok": true}),
        };

        let response = route_call(&MockProvider, request).await;
        assert!(response.success);
        assert!(matches!(
            response.content_items.as_slice(),
            [codex_app_server_protocol::ComputerUseCallOutputContentItem::InputText { text }]
                if text == "mock:click"
        ));
    }
}
