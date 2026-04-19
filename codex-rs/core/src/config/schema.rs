use crate::config::ConfigToml;
use crate::config::types::RawMcpServerConfig;
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
pub(crate) fn features_schema(schema_gen: &mut SchemaGenerator) -> Schema {
    let mut properties = Map::new();
    for feature in FEATURES {
        if feature.id == codex_features::Feature::Artifact {
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
pub(crate) fn mcp_servers_schema(schema_gen: &mut SchemaGenerator) -> Schema {
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

/// Canonicalize a JSON value by sorting its keys.
fn canonicalize(value: &Value) -> Value {
    canonicalize_with_key(/*key*/ None, value)
}

fn canonicalize_with_key(key: Option<&str>, value: &Value) -> Value {
    match value {
        Value::String(text) if key == Some("description") => {
            Value::String(text.split_whitespace().collect::<Vec<_>>().join(" "))
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| canonicalize_with_key(/*key*/ None, item))
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

            let default_is_null = map.get("default").is_some_and(Value::is_null);
            if !default_is_null {
                return;
            }

            if let Some(Value::Array(types)) = map.get_mut("type") {
                types.retain(|ty| ty != "null");
                if types.len() == 1
                    && let Some(single_type) = types.pop()
                {
                    map.insert("type".to_string(), single_type);
                }
            }

            let Some(any_of) = map.remove("anyOf") else {
                return;
            };
            let Value::Array(variants) = any_of else {
                map.insert("anyOf".to_string(), any_of);
                return;
            };

            let mut non_null_variants = Vec::new();
            let mut removed_null_variant = false;
            for variant in variants {
                if is_null_schema_value(&variant) {
                    removed_null_variant = true;
                } else {
                    non_null_variants.push(variant);
                }
            }

            if removed_null_variant && !non_null_variants.is_empty() {
                map.insert("allOf".to_string(), Value::Array(non_null_variants));
            } else {
                map.insert("anyOf".to_string(), Value::Array(non_null_variants));
            }
        }
        _ => {}
    }
}

fn is_null_schema_value(value: &Value) -> bool {
    match value {
        Value::Object(map) => match map.get("type") {
            Some(Value::String(kind)) => kind == "null",
            Some(Value::Array(types)) => !types.is_empty() && types.iter().all(|ty| ty == "null"),
            _ => map.get("const").is_some_and(Value::is_null),
        },
        _ => false,
    }
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
