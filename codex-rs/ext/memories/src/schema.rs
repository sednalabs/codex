use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde_json::Map;
use serde_json::Value;

pub(crate) fn input_schema_for<T: JsonSchema>() -> Value {
    schema_for::<T>(/*option_add_null_type*/ false)
}

pub(crate) fn output_schema_for<T: JsonSchema>() -> Value {
    schema_for::<T>(/*option_add_null_type*/ true)
}

fn schema_for<T: JsonSchema>(option_add_null_type: bool) -> Value {
    let schema = SchemaSettings::draft2019_09()
        .with(|settings| {
            settings.inline_subschemas = true;
        })
        .into_generator()
        .into_root_schema_for::<T>();
    let mut schema_value = serde_json::to_value(schema)
        .unwrap_or_else(|err| panic!("generated tool schema should serialize: {err}"));
    if !option_add_null_type {
        strip_null_type_admissions(&mut schema_value);
    }
    let Value::Object(mut schema_object) = schema_value else {
        unreachable!("root tool schema must be an object");
    };

    let mut tool_schema = Map::new();
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
    Value::Object(tool_schema)
}

// Schemars 1.x always models `Option<T>` as accepting JSON null. Tool inputs
// use omission for optional fields, while tool outputs keep explicit nulls.
fn strip_null_type_admissions(schema: &mut Value) {
    match schema {
        Value::Object(object) => {
            if object
                .get("type")
                .is_some_and(|value| value.as_str() == Some("null"))
            {
                object.remove("type");
            }

            let type_replacement = if let Some(Value::Array(types)) = object.get_mut("type") {
                types.retain(|value| value.as_str() != Some("null"));
                match types.as_slice() {
                    [] => Some(None),
                    [only] => Some(Some(only.clone())),
                    _ => None,
                }
            } else {
                None
            };
            if let Some(replacement) = type_replacement {
                match replacement {
                    Some(value) => {
                        object.insert("type".to_string(), value);
                    }
                    None => {
                        object.remove("type");
                    }
                }
            }

            if object.get("const").is_some_and(Value::is_null) {
                object.remove("const");
            }

            if let Some(Value::Array(values)) = object.get_mut("enum") {
                values.retain(|value| !value.is_null());
                if values.is_empty() {
                    object.remove("enum");
                }
            }

            for key in ["anyOf", "oneOf"] {
                if let Some(Value::Array(schemas)) = object.get_mut(key) {
                    schemas.retain(|schema| !is_null_only_schema(schema));
                }
            }

            for value in object.values_mut() {
                strip_null_type_admissions(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                strip_null_type_admissions(value);
            }
        }
        _ => {}
    }
}

fn is_null_only_schema(schema: &Value) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };

    object
        .get("type")
        .is_some_and(|value| value.as_str() == Some("null"))
        || object.get("const").is_some_and(Value::is_null)
        || object.get("enum").is_some_and(|value| {
            value
                .as_array()
                .is_some_and(|values| values.len() == 1 && values[0].is_null())
        })
}
