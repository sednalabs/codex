use codex_protocol::models::ReasoningItemContent;
use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn message(text: &str) -> ResponseItem {
    serde_json::from_value(json!({
        "type": "message", "role": "user", "content": [{"type": "input_text", "text": text}]
    }))
    .unwrap()
}

fn tool(text: &str) -> ResponseItem {
    serde_json::from_value(json!({
        "type": "function_call_output", "call_id": "call", "output": text
    }))
    .unwrap()
}

#[test]
fn each_kind_evicts_its_own_oldest_entries_without_reordering() {
    let users = [message("first"), message("second"), message("third")];
    let tools: Vec<_> = (0..MAX_ITEMS_PER_KIND)
        .map(|index| tool(&index.to_string()))
        .collect();
    let mut history = TranscriptHistory::default();
    history.record(&users[0]);
    history.record(&tool("old output"));
    history.record(&users[1]);
    for item in &tools {
        history.record(item);
    }
    history.record(&users[2]);
    assert_eq!(
        history.items().collect::<Vec<_>>(),
        users[..2]
            .iter()
            .chain(&tools)
            .chain(&users[2..])
            .collect::<Vec<_>>()
    );
    let generation = history.generation();
    let newer_users: Vec<_> = (0..MAX_ITEMS_PER_KIND)
        .map(|index| message(&index.to_string()))
        .collect();
    for item in &newer_users {
        history.record(item);
    }
    assert_eq!(
        history.items().collect::<Vec<_>>(),
        tools.iter().chain(&newer_users).collect::<Vec<_>>()
    );
    assert!(history.generation() > generation);
}

#[test]
fn byte_limits_are_independent_and_oversized_items_do_not_clear_history() {
    let large = tool(&"x".repeat(MAX_BYTES_PER_KIND / 2));
    let keep = message("keep this");
    let mut history = TranscriptHistory::default();
    history.record(&large);
    history.record(&keep);
    history.record(&large);
    assert_eq!(history.items().collect::<Vec<_>>(), vec![&keep, &large]);

    let oversized = "x".repeat(MAX_BYTES_PER_KIND);
    history.record(&message(&oversized));
    history.record(&tool(&oversized));
    history.record(&ResponseItem::Reasoning {
        id: None,
        summary: Vec::new(),
        // Serialization omits this content, but retention must still count it.
        content: Some(vec![ReasoningItemContent::Text { text: oversized }]),
        encrypted_content: None,
        internal_chat_message_metadata_passthrough: None,
    });
    assert_eq!(history.items().collect::<Vec<_>>(), vec![&keep, &large]);
}
