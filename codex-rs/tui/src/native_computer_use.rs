//! Fixed computer-use provider adapters owned by the TUI.
//!
//! This is intentionally a closed list: each capability keeps its own config,
//! schema, provider and response contract while sharing the TUI registration
//! and asynchronous dispatch lifecycle.

use codex_app_server_protocol::DynamicToolCallParams;
use codex_app_server_protocol::DynamicToolCallResponse;
use codex_app_server_protocol::DynamicToolSpec;
use codex_app_server_protocol::ThreadStartParams;
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NativeComputerUseProvider {
    Browser,
    Android,
    Desktop,
}

impl NativeComputerUseProvider {
    /// Resolve only the fixed native namespaces, preserving their registration
    /// and dispatch order. Provider-specific tool validation remains in each
    /// provider adapter.
    pub(crate) fn for_call(params: &DynamicToolCallParams) -> Option<Self> {
        if crate::browser_dynamic_tools::is_browser_call(params) {
            Some(Self::Browser)
        } else if crate::android_computer_use_provider::is_android_call(params) {
            Some(Self::Android)
        } else if crate::desktop_computer_use_provider::is_desktop_call(params) {
            Some(Self::Desktop)
        } else {
            None
        }
    }

    pub(crate) fn validate_no_reserved_name_conflict(
        params: &ThreadStartParams,
    ) -> Result<(), String> {
        crate::browser_dynamic_tools::validate_no_reserved_name_conflict(params)?;
        crate::android_computer_use_provider::validate_no_reserved_name_conflict(params)?;
        crate::desktop_computer_use_provider::validate_no_reserved_name_conflict(params)
    }

    pub(crate) fn specs_for_codex_home(codex_home: &Path) -> Vec<DynamicToolSpec> {
        let mut specs = crate::browser_dynamic_tools::specs_for_codex_home(codex_home);
        specs.extend(crate::android_computer_use_provider::specs_for_codex_home(
            codex_home,
        ));
        specs.extend(crate::desktop_computer_use_provider::specs_for_codex_home(
            codex_home,
        ));
        specs
    }

    pub(crate) async fn handle_for_codex_home(
        self,
        params: &DynamicToolCallParams,
        codex_home: &Path,
    ) -> DynamicToolCallResponse {
        let (response, unavailable_message) = match self {
            Self::Browser => (
                crate::browser_dynamic_tools::handle_for_codex_home(params, codex_home).await,
                "Browser request did not match an available native tool.",
            ),
            Self::Android => (
                crate::android_computer_use_provider::handle_for_codex_home(params, codex_home)
                    .await,
                "Android request did not match an available native tool.",
            ),
            Self::Desktop => (
                crate::desktop_computer_use_provider::handle_for_codex_home(params, codex_home)
                    .await,
                "Desktop request did not match an available native tool.",
            ),
        };
        response.unwrap_or_else(|| crate::dynamic_tools::failure_response(unavailable_message))
    }

    pub(crate) fn completed_event(
        self,
        request_id: codex_app_server_protocol::RequestId,
        response: DynamicToolCallResponse,
    ) -> crate::app_event::AppEvent {
        match self {
            Self::Browser => crate::browser_dynamic_tools::completed_event(request_id, response),
            Self::Android => {
                crate::android_computer_use_provider::completed_event(request_id, response)
            }
            Self::Desktop => {
                crate::desktop_computer_use_provider::completed_event(request_id, response)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(namespace: Option<&str>) -> DynamicToolCallParams {
        DynamicToolCallParams {
            thread_id: "thread".to_string(),
            turn_id: "turn".to_string(),
            call_id: "call".to_string(),
            namespace: namespace.map(str::to_string),
            tool: "unknown-tool".to_string(),
            arguments: serde_json::Value::Null,
        }
    }

    #[test]
    fn routes_only_the_fixed_provider_namespaces_in_order() {
        assert_eq!(
            NativeComputerUseProvider::for_call(&request(Some("codex_browser"))),
            Some(NativeComputerUseProvider::Browser)
        );
        assert_eq!(
            NativeComputerUseProvider::for_call(&request(Some("codex_android"))),
            Some(NativeComputerUseProvider::Android)
        );
        assert_eq!(
            NativeComputerUseProvider::for_call(&request(Some("codex_desktop"))),
            Some(NativeComputerUseProvider::Desktop)
        );
        assert_eq!(NativeComputerUseProvider::for_call(&request(None)), None);
        assert_eq!(
            NativeComputerUseProvider::for_call(&request(Some("other_namespace"))),
            None
        );
    }
}
