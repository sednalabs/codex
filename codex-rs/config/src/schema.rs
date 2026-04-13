use crate::config_toml::ConfigToml;
use crate::types::RawMcpServerConfig;
use codex_features::FEATURES;
use codex_features::legacy_feature_keys;
use schemars::Schema;
use schemars::SchemaGenerator;
use schemars::generate::SchemaSettings;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::path::Path;

/// Schema for the `[features]` map with known + legacy keys only.
pub fn features_schema(schema_gen: &mut SchemaGenerator) -> Schema {
    let mut properties = Map::new();
    for feature in FEATURES {
        if feature.id == codex_features::Feature::Artifact {
            continue;
        }
        if feature.id == codex_features::Feature::MultiAgentV2 {
            properties.insert(
                feature.key.to_string(),
                schema_gen
                    .subschema_for::<
                        codex_features::FeatureToml<codex_features::MultiAgentV2ConfigToml>,
                    >()
                    .into(),
            );
            continue;
        }
        properties.insert(
            feature.key.to_string(),
            schema_gen.subschema_for::<bool>().into(),
        );
    }
    for legacy_key in legacy_feature_keys() {
        properties.insert(
            legacy_key.to_string(),
            schema_gen.subschema_for::<bool>().into(),
        );
    }

    match json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    })
    .try_into()
    {
        Ok(schema) => schema,
        Err(err) => panic!("features schema should be valid: {err}"),
    }
}

/// Schema for the `[mcp_servers]` map using the raw input shape.
pub fn mcp_servers_schema(schema_gen: &mut SchemaGenerator) -> Schema {
    match json!({
        "type": "object",
        "additionalProperties": schema_gen.subschema_for::<RawMcpServerConfig>(),
    })
    .try_into()
    {
        Ok(schema) => schema,
        Err(err) => panic!("mcp servers schema should be valid: {err}"),
    }
}

/// Build the config schema for `config.toml`.
pub fn config_schema() -> Schema {
    SchemaSettings::draft07()
        .into_generator()
        .into_root_schema_for::<ConfigToml>()
}

fn canonicalize(value: &Value) -> Value {
    canonicalize_with_key(None, value)
}

fn canonicalize_with_key(key: Option<&str>, value: &Value) -> Value {
    match value {
        Value::String(text) if key == Some("description") => {
            Value::String(text.split_whitespace().collect::<Vec<_>>().join(" "))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| canonicalize_with_key(None, item))
                .collect(),
        ),
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|(left, _), (right, _)| left.cmp(right));
            let mut sorted = Map::with_capacity(map.len());
            for (key, child) in entries {
                sorted.insert(
                    key.clone(),
                    canonicalize_with_key(Some(key.as_str()), child),
                );
            }
            Value::Object(sorted)
        }
        _ => value.clone(),
    }
}

/// Render the config schema as pretty-printed JSON.
pub fn config_schema_json() -> anyhow::Result<Vec<u8>> {
    let schema = config_schema();
    let mut value = serde_json::to_value(schema)?;
    normalize_legacy_option_schema(&mut value);
    let value = canonicalize(&value);
    let json = serde_json::to_vec_pretty(&value)?;
    Ok(json)
}

/// Write the config schema fixture to disk.
pub fn write_config_schema(out_path: &Path) -> anyhow::Result<()> {
    let json = config_schema_json()?;
    std::fs::write(out_path, json)?;
    Ok(())
}

fn normalize_legacy_option_schema(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                normalize_legacy_option_schema(item);
            }
        }
        Value::Object(map) => {
            for child in map.values_mut() {
                normalize_legacy_option_schema(child);
            }

            let is_any_of_option =
                map.get("anyOf")
                    .and_then(Value::as_array)
                    .is_some_and(|variants| {
                        variants.len() == 2
                            && variants
                                .iter()
                                .any(|variant| variant == &json!({"type": "null"}))
                    });

            if is_any_of_option && let Some(any_of) = map.remove("anyOf") {
                map.insert("oneOf".to_string(), any_of);
            }
        }
        _ => {}
    }
}
