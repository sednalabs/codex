use codex_protocol::dynamic_tools::DynamicToolCapability;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::protocol::SessionSource;
use codex_tools::ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME;
use codex_tools::ANDROID_OBSERVE_TOOL_NAME;
use codex_tools::ANDROID_STEP_TOOL_NAME;
use codex_tools::load_android_runtime_config;
use serde_json::json;
use std::collections::HashSet;
use std::path::Path;

pub(crate) fn augment_with_acquired_native_android_tools(
    mut dynamic_tools: Vec<DynamicToolSpec>,
    codex_home: &Path,
    session_source: &SessionSource,
) -> Vec<DynamicToolSpec> {
    if !should_acquire_native_android_tools(codex_home, session_source) {
        return dynamic_tools;
    }

    let mut existing_bare_names = dynamic_tools
        .iter()
        .filter(|tool| tool.namespace.is_none())
        .map(|tool| tool.name.clone())
        .collect::<HashSet<_>>();
    for tool in native_android_dynamic_tools() {
        if existing_bare_names.insert(tool.name.clone()) {
            dynamic_tools.push(tool);
        }
    }
    dynamic_tools
}

fn should_acquire_native_android_tools(codex_home: &Path, session_source: &SessionSource) -> bool {
    session_source_supports_native_android_tools(session_source)
        && configured_android_provider_url(codex_home).is_some()
}

fn session_source_supports_native_android_tools(session_source: &SessionSource) -> bool {
    matches!(
        session_source,
        SessionSource::Cli | SessionSource::VSCode | SessionSource::Custom(_)
    )
}

fn configured_android_provider_url(codex_home: &Path) -> Option<String> {
    load_android_runtime_config(codex_home).map(|config| config.mcp_url)
}

fn native_android_dynamic_tools() -> [DynamicToolSpec; 3] {
    [
        native_android_tool(
            ANDROID_OBSERVE_TOOL_NAME,
            "Capture the active Android computer-use screen.",
            "read_only",
            "shared_read",
        ),
        native_android_tool(
            ANDROID_STEP_TOOL_NAME,
            "Perform bounded actions in the active Android computer-use session.",
            "mutating",
            "exclusive_write",
        ),
        native_android_tool(
            ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME,
            "Install a GitHub Actions Android build into the active Android computer-use session.",
            "mutating",
            "exclusive_write",
        ),
    ]
}

fn native_android_tool(
    name: &str,
    description: &str,
    mutation_class: &str,
    lease_mode: &str,
) -> DynamicToolSpec {
    DynamicToolSpec {
        namespace: None,
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {}
        }),
        defer_loading: false,
        persist_on_resume: false,
        capability: Some(DynamicToolCapability {
            family: Some("android".to_string()),
            capability_scope: Some("environment".to_string()),
            mutation_class: Some(mutation_class.to_string()),
            lease_mode: Some(lease_mode.to_string()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[test]
    fn fresh_cli_thread_acquires_native_android_tools_from_runtime_config() {
        let codex_home = tempdir().expect("temp dir");
        std::fs::write(
            codex_home.path().join("android-computer-use.json"),
            r#"{"mcp_url":"https://android.example.test/mcp"}"#,
        )
        .expect("write runtime config");

        let actual = augment_with_acquired_native_android_tools(
            Vec::new(),
            codex_home.path(),
            &SessionSource::Cli,
        );

        assert_eq!(actual, native_android_dynamic_tools());
    }

    #[test]
    fn fresh_thread_without_runtime_config_does_not_acquire_android_tools() {
        let codex_home = tempdir().expect("temp dir");

        let actual = augment_with_acquired_native_android_tools(
            Vec::new(),
            codex_home.path(),
            &SessionSource::Cli,
        );

        assert_eq!(actual, Vec::<DynamicToolSpec>::new());
    }

    #[test]
    fn exec_thread_does_not_acquire_native_android_tools() {
        let codex_home = tempdir().expect("temp dir");
        std::fs::write(
            codex_home.path().join("android-computer-use.json"),
            r#"{"mcp_url":"https://android.example.test/mcp"}"#,
        )
        .expect("write runtime config");

        let actual = augment_with_acquired_native_android_tools(
            Vec::new(),
            codex_home.path(),
            &SessionSource::Exec,
        );

        assert_eq!(actual, Vec::<DynamicToolSpec>::new());
    }

    #[test]
    fn acquired_native_android_tools_do_not_duplicate_explicit_tools() {
        let codex_home = tempdir().expect("temp dir");
        std::fs::write(
            codex_home.path().join("android-computer-use.json"),
            r#"{"mcp_url":"https://android.example.test/mcp"}"#,
        )
        .expect("write runtime config");
        let explicit_observe = native_android_tool(
            ANDROID_OBSERVE_TOOL_NAME,
            "explicit observe",
            "read_only",
            "shared_read",
        );

        let actual = augment_with_acquired_native_android_tools(
            vec![explicit_observe.clone()],
            codex_home.path(),
            &SessionSource::Cli,
        );

        assert_eq!(
            actual,
            vec![
                explicit_observe,
                native_android_tool(
                    ANDROID_STEP_TOOL_NAME,
                    "Perform bounded actions in the active Android computer-use session.",
                    "mutating",
                    "exclusive_write",
                ),
                native_android_tool(
                    ANDROID_INSTALL_BUILD_FROM_RUN_TOOL_NAME,
                    "Install a GitHub Actions Android build into the active Android computer-use session.",
                    "mutating",
                    "exclusive_write",
                ),
            ]
        );
    }
}
