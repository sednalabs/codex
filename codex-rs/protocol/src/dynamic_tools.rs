use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value as JsonValue;
use ts_rs::TS;

#[derive(Debug, Clone, Serialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: JsonValue,
    #[serde(default)]
    pub defer_loading: bool,
    #[serde(default = "default_dynamic_tool_persist_on_resume")]
    pub persist_on_resume: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<DynamicToolCapability>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCapability {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability_scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mutation_class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolCallRequest {
    pub call_id: String,
    pub turn_id: String,
    pub tool: String,
    pub arguments: JsonValue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
pub struct DynamicToolResponse {
    pub content_items: Vec<DynamicToolCallOutputContentItem>,
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type")]
pub enum DynamicToolCallOutputContentItem {
    #[serde(rename_all = "camelCase")]
    InputText { text: String },
    #[serde(rename_all = "camelCase")]
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DynamicToolSpecDe {
    name: String,
    description: String,
    input_schema: JsonValue,
    defer_loading: Option<bool>,
    persist_on_resume: Option<bool>,
    capability: Option<DynamicToolCapability>,
    expose_to_context: Option<bool>,
}

const fn default_dynamic_tool_persist_on_resume() -> bool {
    true
}

impl<'de> Deserialize<'de> for DynamicToolSpec {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let DynamicToolSpecDe {
            name,
            description,
            input_schema,
            defer_loading,
            persist_on_resume,
            capability,
            expose_to_context,
        } = DynamicToolSpecDe::deserialize(deserializer)?;

        Ok(Self {
            name,
            description,
            input_schema,
            defer_loading: defer_loading
                .unwrap_or_else(|| expose_to_context.map(|visible| !visible).unwrap_or(false)),
            persist_on_resume: persist_on_resume
                .unwrap_or(default_dynamic_tool_persist_on_resume()),
            capability,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::DynamicToolCapability;
    use super::DynamicToolSpec;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn dynamic_tool_spec_deserializes_defer_loading() {
        let value = json!({
            "name": "lookup_ticket",
            "description": "Fetch a ticket",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" }
                }
            },
            "deferLoading": true,
        });

        let actual: DynamicToolSpec = serde_json::from_value(value).expect("deserialize");

        assert_eq!(
            actual,
            DynamicToolSpec {
                name: "lookup_ticket".to_string(),
                description: "Fetch a ticket".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" }
                    }
                }),
                defer_loading: true,
                persist_on_resume: true,
                capability: None,
            }
        );
    }

    #[test]
    fn dynamic_tool_spec_legacy_expose_to_context_inverts_to_defer_loading() {
        let value = json!({
            "name": "lookup_ticket",
            "description": "Fetch a ticket",
            "inputSchema": {
                "type": "object",
                "properties": {}
            },
            "exposeToContext": false,
        });

        let actual: DynamicToolSpec = serde_json::from_value(value).expect("deserialize");

        assert!(actual.defer_loading);
        assert!(actual.persist_on_resume);
        assert_eq!(actual.capability, None);
    }

    #[test]
    fn dynamic_tool_spec_deserializes_persist_on_resume() {
        let value = json!({
            "name": "android_observe",
            "description": "observe android",
            "inputSchema": {
                "type": "object",
                "properties": {}
            },
            "persistOnResume": false,
        });

        let actual: DynamicToolSpec = serde_json::from_value(value).expect("deserialize");

        assert!(!actual.persist_on_resume);
        assert_eq!(actual.capability, None);
    }

    #[test]
    fn dynamic_tool_spec_deserializes_capability_metadata() {
        let value = json!({
            "name": "android_step",
            "description": "drive android",
            "inputSchema": {
                "type": "object",
                "properties": {}
            },
            "capability": {
                "family": "android",
                "capabilityScope": "environment",
                "mutationClass": "mutating",
                "leaseMode": "exclusive_write"
            }
        });

        let actual: DynamicToolSpec = serde_json::from_value(value).expect("deserialize");

        assert_eq!(
            actual.capability,
            Some(DynamicToolCapability {
                family: Some("android".to_string()),
                capability_scope: Some("environment".to_string()),
                mutation_class: Some("mutating".to_string()),
                lease_mode: Some("exclusive_write".to_string()),
            })
        );
    }
}
