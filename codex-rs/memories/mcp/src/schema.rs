use rmcp::model::JsonObject;
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde_json::Value as JsonValue;

pub(crate) fn input_schema_for<T: JsonSchema>() -> JsonObject {
    schema_for::<T>(OptionNullability::OmitExplicitNullTypes)
}

pub(crate) fn output_schema_for<T: JsonSchema>() -> JsonObject {
    schema_for::<T>(OptionNullability::IncludeExplicitNullTypes)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionNullability {
    OmitExplicitNullTypes,
    IncludeExplicitNullTypes,
}

impl OptionNullability {
    fn should_strip_explicit_nulls(self) -> bool {
        matches!(self, Self::OmitExplicitNullTypes)
    }
}

fn schema_for<T: JsonSchema>(option_nullability: OptionNullability) -> JsonObject {
    let schema = SchemaSettings::draft2019_09()
        .with(|settings| {
            settings.inline_subschemas = true;
        })
        .into_generator()
        .into_root_schema_for::<T>();
    let schema_value = serde_json::to_value(schema)
        .unwrap_or_else(|err| panic!("generated tool schema should serialize: {err}"));
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
    if option_nullability.should_strip_explicit_nulls() {
        strip_explicit_option_nulls(&mut tool_schema);
    }
    tool_schema
}

fn strip_explicit_option_nulls(schema: &mut JsonObject) {
    strip_explicit_option_nulls_from_object(schema);
}

fn strip_explicit_option_nulls_from_value(value: &mut JsonValue) {
    match value {
        JsonValue::Object(object) => strip_explicit_option_nulls_from_object(object),
        JsonValue::Array(values) => {
            for value in values {
                strip_explicit_option_nulls_from_value(value);
            }
        }
        _ => {}
    }
}

fn strip_explicit_option_nulls_from_object(schema: &mut JsonObject) {
    for value in schema.values_mut() {
        strip_explicit_option_nulls_from_value(value);
    }

    if let Some(type_value) = schema.get_mut("type") {
        strip_null_from_type(type_value);
    }
    if let Some(enum_value) = schema.get_mut("enum") {
        strip_null_from_enum(enum_value);
    }
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(JsonValue::Array(schemas)) = schema.get_mut(keyword) {
            if schemas.len() > 1 {
                schemas.retain(|schema| !is_null_only_schema(schema));
            }
        }
    }
}

fn strip_null_from_type(type_value: &mut JsonValue) {
    let JsonValue::Array(types) = type_value else {
        return;
    };
    if types.iter().any(|ty| ty.as_str() != Some("null")) {
        types.retain(|ty| ty.as_str() != Some("null"));
    }
    if types.len() == 1 {
        if let Some(only_type) = types.pop() {
            *type_value = only_type;
        }
    }
}

fn strip_null_from_enum(enum_value: &mut JsonValue) {
    let JsonValue::Array(values) = enum_value else {
        return;
    };
    if values.len() > 1 && values.iter().any(|value| !value.is_null()) {
        values.retain(|value| !value.is_null());
    }
}

fn is_null_only_schema(schema: &JsonValue) -> bool {
    let JsonValue::Object(schema) = schema else {
        return false;
    };
    schema_type_is_only_null(schema.get("type"))
        || schema_enum_is_only_null(schema.get("enum"))
        || schema.get("const").is_some_and(JsonValue::is_null)
}

fn schema_type_is_only_null(type_value: Option<&JsonValue>) -> bool {
    match type_value {
        Some(JsonValue::String(ty)) => ty == "null",
        Some(JsonValue::Array(types)) if !types.is_empty() => {
            types.iter().all(|ty| ty.as_str() == Some("null"))
        }
        _ => false,
    }
}

fn schema_enum_is_only_null(enum_value: Option<&JsonValue>) -> bool {
    match enum_value {
        Some(JsonValue::Array(values)) if !values.is_empty() => {
            values.iter().all(JsonValue::is_null)
        }
        _ => false,
    }
}
