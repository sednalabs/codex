use crate::android_computer_use_provider::AndroidComputerUseOutcome;
use crate::android_computer_use_provider::handle_android_computer_use;
use crate::browser_computer_use_provider::BrowserComputerUseOutcome;
use crate::browser_computer_use_provider::handle_browser_computer_use_for_codex_home;
use crate::desktop_computer_use_provider::DesktopComputerUseOutcome;
use crate::desktop_computer_use_provider::handle_desktop_computer_use;
use codex_app_server_protocol::ComputerUseCallParams;
use codex_app_server_protocol::ComputerUseCallResponse;
use codex_tools::COMPUTER_USE_ADAPTER_ANDROID;
use codex_tools::COMPUTER_USE_ADAPTER_BROWSER;
use codex_tools::COMPUTER_USE_ADAPTER_DESKTOP;
use codex_tools::native_computer_use_provider_for_call;
use std::path::Path;

pub(crate) enum ComputerUseProviderOutcome {
    Handled(ComputerUseCallResponse),
    Unavailable,
}

pub(crate) async fn handle_computer_use(
    params: &ComputerUseCallParams,
    codex_home: &Path,
) -> ComputerUseProviderOutcome {
    for provider in computer_use_providers() {
        if provider.supports(params) {
            return provider.handle(params, codex_home).await;
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

    async fn handle(
        &self,
        params: &ComputerUseCallParams,
        codex_home: &Path,
    ) -> ComputerUseProviderOutcome {
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
                match handle_browser_computer_use_for_codex_home(params, codex_home).await {
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
    use std::path::Path;
    use tempfile::tempdir;

    #[tokio::test]
    async fn browser_provider_requires_configured_backend() {
        let outcome = handle_computer_use(
            &ComputerUseCallParams {
                thread_id: "thread-1".to_string(),
                call_id: "call-browser-observe".to_string(),
                turn_id: "turn-1".to_string(),
                environment_id: Some("env-1".to_string()),
                adapter: "browser".to_string(),
                tool: "browser_observe".to_string(),
                arguments: json!({"scope": "viewport_and_page"}),
            },
            Path::new("/nonexistent-codex-home"),
        )
        .await;

        assert!(matches!(outcome, ComputerUseProviderOutcome::Unavailable));
    }

    #[tokio::test]
    async fn unknown_computer_use_tool_is_not_claimed_by_provider_registry() {
        let outcome = handle_computer_use(
            &ComputerUseCallParams {
                thread_id: "thread-1".to_string(),
                call_id: "call-unknown".to_string(),
                turn_id: "turn-1".to_string(),
                environment_id: Some("env-1".to_string()),
                adapter: "browser".to_string(),
                tool: "browser_private_backend_probe".to_string(),
                arguments: json!({}),
            },
            Path::new("/nonexistent-codex-home"),
        )
        .await;

        assert!(matches!(outcome, ComputerUseProviderOutcome::Unavailable));
    }

    #[tokio::test]
    async fn desktop_provider_requires_configured_command() {
        let outcome = handle_computer_use(
            &ComputerUseCallParams {
                thread_id: "thread-1".to_string(),
                call_id: "call-desktop-observe".to_string(),
                turn_id: "turn-1".to_string(),
                environment_id: Some("env-1".to_string()),
                adapter: "desktop".to_string(),
                tool: "desktop_observe".to_string(),
                arguments: json!({"scope": "screen_and_ui"}),
            },
            Path::new("/nonexistent-codex-home"),
        )
        .await;

        assert!(matches!(outcome, ComputerUseProviderOutcome::Unavailable));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn browser_provider_executes_from_explicit_session_codex_home() {
        let ambient_home = tempdir().expect("ambient codex home");
        let child_home = tempdir().expect("child codex home");
        let config = r#"{"provider":"command","command":["sh","-c","cat >/dev/null; printf '{\"contentItems\":[{\"type\":\"inputText\",\"text\":\"child home\"},{\"type\":\"inputImage\",\"imageUrl\":\"data:image/png;base64,AAAA\",\"detail\":\"high\"}],\"success\":true}"]}"#;
        std::fs::write(
            ambient_home.path().join("browser-computer-use.json"),
            config.replace("child home", "ambient home"),
        )
        .expect("write ambient browser provider config");
        std::fs::write(child_home.path().join("browser-computer-use.json"), config)
            .expect("write child browser provider config");

        let outcome = handle_computer_use(
            &ComputerUseCallParams {
                thread_id: "thread-1".to_string(),
                call_id: "call-browser-observe".to_string(),
                turn_id: "turn-1".to_string(),
                environment_id: Some("env-1".to_string()),
                adapter: "browser".to_string(),
                tool: "browser_observe".to_string(),
                arguments: json!({}),
            },
            child_home.path(),
        )
        .await;

        let ComputerUseProviderOutcome::Handled(response) = outcome else {
            panic!("child session browser provider should handle the request");
        };
        assert!(response.success);
        assert_eq!(
            response.content_items,
            vec![
                codex_app_server_protocol::ComputerUseCallOutputContentItem::InputText {
                    text: "child home".to_string(),
                },
                codex_app_server_protocol::ComputerUseCallOutputContentItem::InputImage {
                    image_url: "data:image/png;base64,AAAA".to_string(),
                    detail: Some("high".to_string()),
                }
            ]
        );
    }
}
