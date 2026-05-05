use rmcp::model::JsonObject;
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde_json::Value;

pub(crate) fn input_schema_for<T: JsonSchema>() -> JsonObject {
    schema_for::<T>(/*option_add_null_type*/ false)
}

pub(crate) fn output_schema_for<T: JsonSchema>() -> JsonObject {
    schema_for::<T>(/*option_add_null_type*/ true)
}

fn schema_for<T: JsonSchema>(option_add_null_type: bool) -> JsonObject {
    let schema = SchemaSettings::draft2019_09()
        .with(|settings| {
            settings.inline_subschemas = true;
        })
        .into_generator()
        .into_root_schema_for::<T>();
    let mut schema_value = serde_json::to_value(schema)
        .unwrap_or_else(|err| panic!("generated tool schema should serialize: {err}"));
    if !option_add_null_type {
        strip_null_type_allowances(&mut schema_value);
    }
    let serde_json::Value::Object(mut schema_object) = schema_value else {
        unreachable!("root tool schema must be an object");
    };

    // MCP tools only need the JSON Schema body, not schemars' root metadata.
    let mut tool_schema = JsonObject::new();
    for key in [
        "properties",
        "required",
        "type",
        "additionalProperties",
        "$defs",
        "definitions",
    ] {
        if let Some(value) = schema_object.remove(key) {
            tool_schema.insert(key.to_string(), value);
        }
    }
    tool_schema
}

fn strip_null_type_allowances(value: &mut Value) {
    match value {
        Value::Object(object) => {
            if let Some(Value::Array(types)) = object.get_mut("type") {
                types.retain(|ty| ty.as_str() != Some("null"));
                if types.len() == 1 {
                    if let Some(ty) = types.pop() {
                        object.insert("type".to_string(), ty);
                    }
                }
            }

            for key in ["anyOf", "oneOf"] {
                if let Some(Value::Array(schemas)) = object.get_mut(key) {
                    schemas.retain(|schema| !is_null_schema(schema));
                }
            }

            if let Some(Value::Array(variants)) = object.get_mut("enum") {
                variants.retain(|variant| !variant.is_null());
            }

            for child in object.values_mut() {
                strip_null_type_allowances(child);
            }
        }
        Value::Array(items) => {
            for item in items {
                strip_null_type_allowances(item);
            }
        }
        _ => {}
    }
}

fn is_null_schema(value: &Value) -> bool {
    value
        .as_object()
        .and_then(|object| object.get("type"))
        .is_some_and(|ty| ty.as_str() == Some("null"))
}
