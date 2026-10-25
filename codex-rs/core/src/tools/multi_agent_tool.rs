//! Applies captured Multi-Agent V2 catalog overrides and namespaces to tool specifications.
//! Parameter schemas retain harness-owned encryption annotations; execution is unchanged.

use crate::session::session::Session;
use crate::tools::context::ToolInvocation;
use crate::tools::registry::CoreToolRuntime;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ToolExecutor;
use codex_tools::ToolExposure;
use codex_tools::ToolName;
use codex_tools::ToolSearchInfo;
use codex_tools::ToolSpec;
use futures::future::BoxFuture;
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) const MULTI_AGENT_V2_NAMESPACE_DESCRIPTION: &str =
    "Tools for spawning and managing sub-agents.";

pub(super) fn multi_agent_v2_handler(
    handler: impl CoreToolRuntime + 'static,
    namespace: Option<&str>,
    description_override: Option<&str>,
    parameters_override: Option<&str>,
    mailbox_enabled: bool,
) -> Arc<dyn CoreToolRuntime> {
    let tool_name = handler.tool_name();
    // Mailbox-native fields are host-owned additions: enabled schemas stay authoritative
    // through catalog overrides, while the disabled path keeps upstream replacement behavior.
    let mailbox_native_fields: &[&str] = match tool_name.name.as_str() {
        "wait_agent" => &["targets", "replies_to", "return_when"],
        "send_message" => &["kind", "reply_to"],
        _ => &[],
    };
    let mailbox_native = (mailbox_enabled && !mailbox_native_fields.is_empty())
        .then(|| handler.spec())
        .and_then(|spec| match spec {
            ToolSpec::Function(tool)
                if mailbox_native_fields.iter().all(|name| {
                    tool.parameters
                        .properties
                        .as_ref()
                        .is_some_and(|properties| properties.contains_key(*name))
                }) =>
            {
                Some((tool.description, tool.parameters))
            }
            _ => None,
        });
    let mailbox_native_description = mailbox_native
        .as_ref()
        .map(|(description, _)| description.clone());
    let mailbox_native_parameters = mailbox_native.map(|(_, parameters)| parameters);
    let parameters_override = parameters_override.map(|parameters| -> Result<JsonSchema, &str> {
        let mut parameters = codex_tools::parse_catalog_tool_parameters(parameters)?;
        if let ToolSpec::Function(tool) = handler.spec()
            && let Some(properties) = tool.parameters.properties
        {
            // Argument transport requires these markers even without server encryption config.
            for (name, schema) in *properties {
                if schema.encrypted == Some(true) {
                    let property = parameters
                        .properties
                        .as_mut()
                        .and_then(|properties| properties.get_mut(&name))
                        .ok_or("schema omits an encrypted parameter")?;
                    property.encrypted = Some(true);
                }
            }
        }
        if let Some(canonical) = &mailbox_native_parameters {
            parameters =
                merge_mailbox_native_parameters(parameters, canonical, mailbox_native_fields);
        }
        Ok(parameters)
    });
    if let Some(Err(reason)) = &parameters_override {
        tracing::warn!(tool = %handler.tool_name(), reason, "Invalid catalog tool parameters; using bundled parameters");
    }
    let parameters_override = parameters_override.and_then(Result::ok);
    let description_override = description_override.map(|description| {
        mailbox_native_description.as_deref().map_or_else(
            || description.to_owned(),
            |canonical| append_mailbox_native_guidance(description, canonical),
        )
    });
    if namespace.is_none() && description_override.is_none() && parameters_override.is_none() {
        return Arc::new(handler);
    }
    Arc::new(MultiAgentV2ToolOverrides {
        handler: Arc::new(handler),
        namespace: namespace.map(str::to_owned),
        description_override,
        parameters_override,
    })
}

fn merge_mailbox_native_parameters(
    mut catalog: JsonSchema,
    canonical: &JsonSchema,
    fields: &[&str],
) -> JsonSchema {
    let Some(canonical_properties) = canonical.properties.as_ref() else {
        return catalog;
    };
    let properties = catalog
        .properties
        .get_or_insert_with(|| Box::new(BTreeMap::new()));
    for name in fields {
        if let Some(schema) = canonical_properties.get(*name) {
            properties.insert(name.to_string(), schema.clone());
        }
    }
    catalog.schema_type.clone_from(&canonical.schema_type);
    catalog
        .additional_properties
        .clone_from(&canonical.additional_properties);
    if let Some(required) = catalog.required.take() {
        let required = required
            .into_vec()
            .into_iter()
            .filter(|name| !fields.contains(&name.as_str()))
            .collect::<Vec<_>>();
        if !required.is_empty() {
            catalog.required = Some(required.into_boxed_slice());
        }
    }
    catalog
}

fn append_mailbox_native_guidance(description: &str, canonical: &str) -> String {
    if description.contains(canonical) {
        description.to_owned()
    } else if description.is_empty() {
        canonical.to_owned()
    } else {
        format!("{description}\n\n{canonical}")
    }
}

struct MultiAgentV2ToolOverrides {
    handler: Arc<dyn CoreToolRuntime>,
    namespace: Option<String>,
    description_override: Option<String>,
    parameters_override: Option<JsonSchema>,
}

impl ToolExecutor<ToolInvocation> for MultiAgentV2ToolOverrides {
    fn tool_name(&self) -> ToolName {
        let tool_name = self.handler.tool_name();
        match &self.namespace {
            Some(namespace) => ToolName::namespaced(namespace.clone(), tool_name.name),
            None => tool_name,
        }
    }

    fn spec(&self) -> ToolSpec {
        let mut spec = self.handler.spec();
        if let ToolSpec::Function(tool) = &mut spec {
            if let Some(description) = &self.description_override {
                tool.description.clone_from(description);
            }
            if let Some(parameters) = &self.parameters_override {
                tool.parameters.clone_from(parameters);
            }
        }
        match (&self.namespace, spec) {
            (Some(namespace), ToolSpec::Function(tool)) => {
                ToolSpec::Namespace(ResponsesApiNamespace {
                    name: namespace.clone(),
                    description: MULTI_AGENT_V2_NAMESPACE_DESCRIPTION.to_string(),
                    tools: vec![ResponsesApiNamespaceTool::Function(tool)],
                })
            }
            (_, spec) => spec,
        }
    }

    fn exposure(&self) -> ToolExposure {
        self.handler.exposure()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        self.handler.supports_parallel_tool_calls()
    }

    fn search_info(&self) -> Option<ToolSearchInfo> {
        self.handler.search_info()
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        self.handler.handle(invocation)
    }
}

impl CoreToolRuntime for MultiAgentV2ToolOverrides {
    fn wait_until_ready<'a>(&'a self, session: &'a Arc<Session>) -> Option<BoxFuture<'a, ()>> {
        self.handler.wait_until_ready(session)
    }

    fn matches_kind(&self, payload: &crate::tools::context::ToolPayload) -> bool {
        self.handler.matches_kind(payload)
    }

    fn create_diff_consumer(
        &self,
    ) -> Option<Box<dyn crate::tools::registry::ToolArgumentDiffConsumer>> {
        self.handler.create_diff_consumer()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::handlers::multi_agents_spec::WAIT_AGENT_TOOL_DESCRIPTION_MAILBOX;
    use crate::tools::handlers::multi_agents_spec::WaitAgentTimeoutOptions;
    use crate::tools::handlers::multi_agents_v2::WaitAgentHandler;
    use codex_tools::ResponsesApiNamespaceTool;
    use codex_tools::collect_code_mode_exec_prompt_tool_definitions;
    use serde_json::json;

    fn wait_options() -> WaitAgentTimeoutOptions {
        WaitAgentTimeoutOptions {
            default_timeout_ms: 30_000,
            min_timeout_ms: 10_000,
            max_timeout_ms: 3_600_000,
        }
    }

    fn namespaced_wait_function(spec: ToolSpec) -> codex_tools::ResponsesApiTool {
        let ToolSpec::Namespace(namespace) = spec else {
            panic!("expected wait_agent namespace");
        };
        let Some(ResponsesApiNamespaceTool::Function(tool)) = namespace
            .tools
            .into_iter()
            .find(|tool| matches!(tool, ResponsesApiNamespaceTool::Function(tool) if tool.name == "wait_agent"))
        else {
            panic!("expected namespaced wait_agent function");
        };
        tool
    }

    #[test]
    fn mailbox_wait_catalog_override_is_canonical_in_responses_and_code_mode() {
        let catalog_parameters = json!({
            "type": "object",
            "properties": {
                "targets": {"type": "string", "description": "stale target schema"},
                "timeout_ms": {"type": "integer", "description": "catalog timeout"},
                "catalog_tag": {"type": "string"}
            },
            "required": ["targets", "catalog_tag"],
            "additionalProperties": false
        })
        .to_string();
        let handler = WaitAgentHandler::new(wait_options(), /*mailbox_enabled*/ true);
        let wrapped = multi_agent_v2_handler(
            handler,
            Some("collaboration"),
            Some("Catalog-specific wait description."),
            Some(&catalog_parameters),
            /*mailbox_enabled*/ true,
        );
        let spec = wrapped.spec();
        let tool = namespaced_wait_function(spec.clone());
        let properties = tool.parameters.properties.as_ref().unwrap();
        assert!(
            tool.description
                .contains("Catalog-specific wait description.")
        );
        assert!(
            tool.description
                .contains(WAIT_AGENT_TOOL_DESCRIPTION_MAILBOX)
        );
        assert_eq!(
            properties["targets"].schema_type,
            canonical_array_schema().schema_type
        );
        assert_eq!(
            properties["targets"].description.as_deref(),
            Some("Optional agent ids or exact work references to join.")
        );
        assert!(properties.contains_key("replies_to"));
        assert!(properties.contains_key("return_when"));
        assert_eq!(
            properties["timeout_ms"].description.as_deref(),
            Some("catalog timeout")
        );
        assert!(properties.contains_key("catalog_tag"));
        assert_eq!(
            tool.parameters.required.as_deref(),
            Some(["catalog_tag".to_string()].as_slice())
        );

        let code_mode = collect_code_mode_exec_prompt_tool_definitions([&spec], Some(64 * 1024));
        let definition = code_mode
            .iter()
            .find(|definition| {
                definition.tool_name.namespace.as_deref() == Some("collaboration")
                    && definition.tool_name.name == "wait_agent"
            })
            .expect("Code Mode should advertise the namespaced wait_agent definition");
        let advertised = definition.input_schema.as_ref().expect("wait input schema");
        assert!(advertised["properties"].get("targets").is_some());
        assert!(advertised["properties"].get("replies_to").is_some());
        assert!(
            definition
                .description
                .contains(WAIT_AGENT_TOOL_DESCRIPTION_MAILBOX)
        );
    }

    #[test]
    fn mailbox_wait_catalog_override_remains_replacement_when_mailbox_is_disabled() {
        let catalog_parameters = json!({
            "type": "object",
            "properties": {"catalog_tag": {"type": "string"}},
            "additionalProperties": false
        })
        .to_string();
        let handler = WaitAgentHandler::new(wait_options(), /*mailbox_enabled*/ false);
        let wrapped = multi_agent_v2_handler(
            handler,
            Some("collaboration"),
            Some("Catalog-specific wait description."),
            Some(&catalog_parameters),
            /*mailbox_enabled*/ false,
        );
        let tool = namespaced_wait_function(wrapped.spec());
        let properties = tool.parameters.properties.as_ref().unwrap();
        assert_eq!(
            properties.keys().map(String::as_str).collect::<Vec<_>>(),
            ["catalog_tag"]
        );
        assert_eq!(tool.description, "Catalog-specific wait description.");
    }

    fn canonical_array_schema() -> JsonSchema {
        JsonSchema::array(JsonSchema::string(None), None)
    }
}
