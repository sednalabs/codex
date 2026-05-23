use crate::android_computer_use_provider::AndroidComputerUseOutcome;
use crate::android_computer_use_provider::handle_android_computer_use;
use codex_app_server_protocol::ComputerUseCallParams;
use codex_app_server_protocol::ComputerUseCallResponse;

const ADAPTER_ANDROID: &str = "android";
const ADAPTER_BROWSER: &str = "browser";
const TOOL_ANDROID_INSTALL_BUILD_FROM_RUN: &str = "android_install_build_from_run";
const TOOL_ANDROID_OBSERVE: &str = "android_observe";
const TOOL_ANDROID_STEP: &str = "android_step";
const TOOL_BROWSER_OBSERVE: &str = "browser_observe";
const TOOL_BROWSER_STEP: &str = "browser_step";

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

fn computer_use_providers() -> [RegisteredComputerUseProvider; 2] {
    [
        RegisteredComputerUseProvider::Android,
        RegisteredComputerUseProvider::Browser,
    ]
}

enum RegisteredComputerUseProvider {
    Android,
    Browser,
}

impl RegisteredComputerUseProvider {
    fn supports(&self, params: &ComputerUseCallParams) -> bool {
        match self {
            Self::Android => {
                params.adapter == ADAPTER_ANDROID
                    && matches!(
                        params.tool.as_str(),
                        TOOL_ANDROID_OBSERVE
                            | TOOL_ANDROID_STEP
                            | TOOL_ANDROID_INSTALL_BUILD_FROM_RUN
                    )
            }
            Self::Browser => {
                params.adapter == ADAPTER_BROWSER
                    && matches!(
                        params.tool.as_str(),
                        TOOL_BROWSER_OBSERVE | TOOL_BROWSER_STEP
                    )
            }
        }
    }

    async fn handle(&self, params: &ComputerUseCallParams) -> ComputerUseProviderOutcome {
        match self {
            Self::Android => match handle_android_computer_use(params).await {
                AndroidComputerUseOutcome::Handled(response) => {
                    ComputerUseProviderOutcome::Handled(response)
                }
                AndroidComputerUseOutcome::Unavailable => ComputerUseProviderOutcome::Unavailable,
            },
            Self::Browser => ComputerUseProviderOutcome::Unavailable,
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
    async fn browser_provider_is_a_known_but_unavailable_adapter_until_backend_is_connected() {
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
}
