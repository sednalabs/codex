//! Compare durable fingerprints without losing unknown historical data.
//!
//! Schema-v5 mailboxes may contain host metadata that the current protocol no
//! longer represents. Keep stored payloads untouched and ignore only the known
//! retired fields, at the communication object for this exact fingerprint.

use serde_json::Value;

const RETIRED_METADATA_KEYS: &[&str] = &[
    "create_time",
    "content_item_kinds",
    "content_item_metadata",
    "cell_id",
    "executed_tool_calls",
    "tool_calls_complete",
];

pub(crate) fn equivalent(old: &str, new: &str, communication_field: &str) -> bool {
    if old == new {
        return true;
    }
    let (Ok(mut old), Ok(mut new)) = (
        serde_json::from_str::<Value>(old),
        serde_json::from_str::<Value>(new),
    ) else {
        return false;
    };
    if let Some(metadata) = old
        .get_mut(communication_field)
        .and_then(|communication| {
            communication.get_mut("internal_chat_message_metadata_passthrough")
        })
        .and_then(Value::as_object_mut)
    {
        for key in RETIRED_METADATA_KEYS {
            metadata.remove(*key);
        }
    }
    // Schema-v5 terminal outcomes preceded the native result-intent fields.
    // Treat only their exact new terminal defaults as equivalent to absence;
    // other explicit intent/origin values remain significant.
    if communication_field == "response" {
        if let (Some(old_response), Some(new_response)) = (
            old.get("response").and_then(Value::as_object),
            new.get_mut("response").and_then(Value::as_object_mut),
        ) {
            for (key, terminal_default) in [("intent", "result_ready"), ("origin", "result")] {
                if !old_response.contains_key(key)
                    && new_response.get(key).and_then(Value::as_str) == Some(terminal_default)
                {
                    new_response.remove(key);
                }
            }
        }
    }
    old == new
}

#[cfg(test)]
mod tests {
    use super::equivalent;

    #[test]
    fn ignores_only_retired_metadata_at_designated_path() {
        let current = r#"{"input":{"content":"hello","internal_chat_message_metadata_passthrough":{"turn_id":"turn"}}}"#;
        let historical = r#"{"input":{"content":"hello","internal_chat_message_metadata_passthrough":{"turn_id":"turn","create_time":123,"content_item_kinds":["text"],"content_item_metadata":{},"cell_id":"cell","executed_tool_calls":[],"tool_calls_complete":true}}}"#;
        assert!(equivalent(historical, current, "input"));
        assert!(!equivalent(historical, &current.replace("hello", "changed"), "input"));
        assert!(!equivalent(historical, &current.replace("turn\"", "other\""), "input"));
        assert!(!equivalent(historical, current, "communication"));
        let unknown = historical.replace("\"create_time\":123", "\"unknown\":123");
        assert!(!equivalent(&unknown, current, "input"));
        assert!(!equivalent("invalid", current, "input"));
        for field in ["communication", "response"] {
            let old = historical.replace("\"input\"", &format!("\"{field}\""));
            let new = current.replace("\"input\"", &format!("\"{field}\""));
            assert!(equivalent(&old, &new, field));
        }
        let outside = historical.replace("\"input\":{", "\"create_time\":123,\"input\":{");
        assert!(!equivalent(&outside, current, "input"));
        // A future format that supplies a retired field must not silently
        // ignore the new value; only the persisted historical side is reduced.
        assert!(!equivalent(current, historical, "input"));
        let old_outcome = r#"{"response":{"content":"done"}}"#;
        let new_outcome = r#"{"response":{"content":"done","intent":"result_ready","origin":"result"}}"#;
        assert!(equivalent(old_outcome, new_outcome, "response"));
        assert!(!equivalent(old_outcome, &new_outcome.replace("result_ready", "action_required"), "response"));
    }
}
