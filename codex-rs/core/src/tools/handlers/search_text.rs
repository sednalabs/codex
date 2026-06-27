use serde_json::Map;
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Default)]
pub(super) struct SearchTextBuilder {
    parts: Vec<String>,
    seen: BTreeSet<String>,
}

impl SearchTextBuilder {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn push(&mut self, value: impl AsRef<str>) {
        let value = normalize_whitespace(value.as_ref());
        if value.is_empty() {
            return;
        }
        self.push_unique(value.clone());

        if !should_add_identifier_variants(&value) {
            return;
        }

        let tokens = identifier_tokens(&value);
        if tokens.len() <= 1 {
            return;
        }

        self.push_unique(tokens.join(" "));
        self.push_unique(tokens.join("_"));
        self.push_unique(tokens.join("-"));
        self.push_unique(tokens.join(""));
    }

    pub(super) fn push_schema_terms(&mut self, schema: &Value) {
        if let Value::Object(schema) = schema {
            self.push_schema_object_terms(schema);
        }
    }

    pub(super) fn push_schema_object_terms(&mut self, schema: &Map<String, Value>) {
        push_schema_string_field(self, schema, "title");
        push_schema_string_field(self, schema, "description");
        if let Some(properties) = schema.get("properties").and_then(Value::as_object) {
            let mut property_names = properties.keys().collect::<Vec<_>>();
            property_names.sort();
            for property_name in property_names {
                self.push(property_name);
                if let Some(Value::Object(property_schema)) = properties.get(property_name) {
                    push_schema_string_field(self, property_schema, "title");
                    push_schema_string_field(self, property_schema, "description");
                }
            }
        }
    }

    pub(super) fn finish(self) -> String {
        self.parts.join(" ")
    }

    fn push_unique(&mut self, value: String) {
        if self.seen.insert(value.clone()) {
            self.parts.push(value);
        }
    }
}

fn push_schema_string_field(
    builder: &mut SearchTextBuilder,
    schema: &Map<String, Value>,
    field: &str,
) {
    if let Some(value) = schema.get(field).and_then(Value::as_str) {
        builder.push(value);
    }
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn should_add_identifier_variants(value: &str) -> bool {
    value.chars().any(|ch| matches!(ch, '_' | '-' | ':' | '/'))
        || !value.contains(char::is_whitespace) && has_internal_boundary(value)
}

fn has_internal_boundary(value: &str) -> bool {
    let chars = value.chars().collect::<Vec<_>>();
    (1..chars.len()).any(|idx| starts_new_token(&chars, idx))
}

fn identifier_tokens(value: &str) -> Vec<String> {
    let chars = value.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut current = String::new();

    for idx in 0..chars.len() {
        let ch = chars[idx];
        if !ch.is_ascii_alphanumeric() {
            push_token(&mut tokens, &mut current);
            continue;
        }

        if starts_new_token(&chars, idx) {
            push_token(&mut tokens, &mut current);
        }
        current.push(ch.to_ascii_lowercase());
    }
    push_token(&mut tokens, &mut current);

    tokens
}

fn starts_new_token(chars: &[char], idx: usize) -> bool {
    if idx == 0 {
        return false;
    }
    let ch = chars[idx];
    let prev = chars[idx - 1];
    if !prev.is_ascii_alphanumeric() {
        return false;
    }

    if ch.is_ascii_alphabetic() && prev.is_ascii_digit() {
        return true;
    }

    if ch.is_ascii_uppercase() && prev.is_ascii_lowercase() {
        return true;
    }

    if ch.is_ascii_uppercase()
        && prev.is_ascii_uppercase()
        && chars
            .get(idx + 1)
            .is_some_and(char::is_ascii_lowercase)
    {
        return true;
    }

    false
}

fn push_token(tokens: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn search_text_builder_adds_identifier_variants() {
        let mut builder = SearchTextBuilder::new();
        builder.push("d1_query_read_only");
        builder.push("QueryD1Database");

        assert_eq!(
            builder.finish(),
            "d1_query_read_only d1 query read only d1-query-read-only d1queryreadonly QueryD1Database query d1 database query_d1_database query-d1-database queryd1database"
        );
    }
}
