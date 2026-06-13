use crate::android_computer_use_provider::AndroidComputerUseOutcome;
use crate::android_computer_use_provider::handle_android_computer_use;
use crate::browser_computer_use_provider::BrowserComputerUseOutcome;
use crate::browser_computer_use_provider::handle_browser_computer_use;
use crate::desktop_computer_use_provider::DesktopComputerUseOutcome;
use crate::desktop_computer_use_provider::handle_desktop_computer_use;
use codex_app_server_protocol::ComputerUseCallParams;
use codex_app_server_protocol::ComputerUseCallResponse;
use codex_tools::COMPUTER_USE_ADAPTER_ANDROID;
use codex_tools::COMPUTER_USE_ADAPTER_BROWSER;
use codex_tools::COMPUTER_USE_ADAPTER_DESKTOP;
use codex_tools::native_computer_use_provider_for_call;

pub(crate) enum ComputerUseProviderOutcome {
    Handled(ComputerUseCallResponse),
    Unavailable,
}

pub(crate) async fn handle_computer_use(
    params: &ComputerUseCallParams,
) -> ComputerUseProviderOutcome {
    for provider in computer_use_providers() {
        if provider.supports(params) {
            return provider.handle(params).await;
        }
    }
    ComputerUseProviderOutcome::Unavailable
}

fn computer_use_providers() -> &'static [RegisteredComputerUseProvider] {
    static PROVIDERS: [RegisteredComputerUseProvider; 3] = [
        RegisteredComputerUseProvider {
            adapter: COMPUTER_USE_ADAPTER_ANDROID,
            handler: ComputerUseProviderHandler::Android,
        },
        RegisteredComputerUseProvider {
            adapter: COMPUTER_USE_ADAPTER_BROWSER,
            handler: ComputerUseProviderHandler::Browser,
        },
        RegisteredComputerUseProvider {
            adapter: COMPUTER_USE_ADAPTER_DESKTOP,
            handler: ComputerUseProviderHandler::Desktop,
        },
    ];

    &PROVIDERS
}

#[derive(Clone, Copy)]
struct RegisteredComputerUseProvider {
    adapter: &'static str,
    handler: ComputerUseProviderHandler,
}

#[derive(Clone, Copy)]
enum ComputerUseProviderHandler {
    Android,
    Browser,
    Desktop,
}

impl RegisteredComputerUseProvider {
    fn supports(&self, params: &ComputerUseCallParams) -> bool {
        native_computer_use_provider_for_call(&params.adapter, &params.tool)
            .is_some_and(|(provider, _)| provider.adapter == self.adapter)
    }

    async fn handle(&self, params: &ComputerUseCallParams) -> ComputerUseProviderOutcome {
        match self.handler {
            ComputerUseProviderHandler::Android => {
                match handle_android_computer_use(params).await {
                    AndroidComputerUseOutcome::Handled(response) => {
                        ComputerUseProviderOutcome::Handled(response)
                    }
                    AndroidComputerUseOutcome::Unavailable => {
                        ComputerUseProviderOutcome::Unavailable
                    }
                }
            }
            ComputerUseProviderHandler::Browser => {
                match handle_browser_computer_use(params).await {
                    BrowserComputerUseOutcome::Handled(response) => {
                        ComputerUseProviderOutcome::Handled(response)
                    }
                    BrowserComputerUseOutcome::Unavailable => {
                        ComputerUseProviderOutcome::Unavailable
                    }
                }
            }
            ComputerUseProviderHandler::Desktop => {
                match handle_desktop_computer_use(params).await {
                    DesktopComputerUseOutcome::Handled(response) => {
                        ComputerUseProviderOutcome::Handled(response)
                    }
                    DesktopComputerUseOutcome::Unavailable => {
                        ComputerUseProviderOutcome::Unavailable
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ComputerUseProviderOutcome;
    use super::handle_computer_use;
    use codex_app_server_protocol::ComputerUseCallParams;
    use serde_json::json;

    #[tokio::test]
    async fn browser_provider_requires_configured_backend() {
        let outcome = handle_computer_use(&ComputerUseCallParams {
            thread_id: "thread-1".to_string(),
            call_id: "call-browser-observe".to_string(),
            turn_id: "turn-1".to_string(),
            environment_id: Some("env-1".to_string()),
            adapter: "browser".to_string(),
            tool: "browser_observe".to_string(),
            arguments: json!({"scope": "viewport_and_page"}),
        })
        .await;

        assert!(matches!(outcome, ComputerUseProviderOutcome::Unavailable));
    }

    #[tokio::test]
    async fn unknown_computer_use_tool_is_not_claimed_by_provider_registry() {
        let outcome = handle_computer_use(&ComputerUseCallParams {
            thread_id: "thread-1".to_string(),
            call_id: "call-unknown".to_string(),
            turn_id: "turn-1".to_string(),
            environment_id: Some("env-1".to_string()),
            adapter: "browser".to_string(),
            tool: "browser_private_backend_probe".to_string(),
            arguments: json!({}),
        })
        .await;

        assert!(matches!(outcome, ComputerUseProviderOutcome::Unavailable));
    }

    #[tokio::test]
    async fn desktop_provider_requires_configured_command() {
        let outcome = handle_computer_use(&ComputerUseCallParams {
            thread_id: "thread-1".to_string(),
            call_id: "call-desktop-observe".to_string(),
            turn_id: "turn-1".to_string(),
            environment_id: Some("env-1".to_string()),
            adapter: "desktop".to_string(),
            tool: "desktop_observe".to_string(),
            arguments: json!({"scope": "screen_and_ui"}),
        })
        .await;

        assert!(matches!(outcome, ComputerUseProviderOutcome::Unavailable));
    }
}
