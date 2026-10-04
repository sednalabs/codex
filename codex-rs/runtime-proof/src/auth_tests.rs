//! Protected-failure and per-invocation secret-redaction controls.

use super::McpRedactionContext;
use super::failed_target_match;
use pretty_assertions::assert_eq;
use zeroize::Zeroizing;

#[test]
fn protected_runtime_failure_never_downgrades_to_unconfigured_target() {
    assert!(failed_target_match("other", "https://other.invalid/mcp").is_err());
}

#[test]
fn redaction_scrubs_json_values_and_keys_without_overwriting_collisions() {
    let context = McpRedactionContext {
        bearer: Zeroizing::new("private-bearer".to_string()),
        proof_tokens: vec![Zeroizing::new("private-proof".to_string())],
    };
    let mut value = serde_json::json!({
        "private-bearer": "private-proof",
        "[runtime secret redacted]": "keep",
        "nested": [{"echo": "Bearer private-bearer"}]
    });
    context.redact(&mut value);
    assert_eq!(
        value,
        serde_json::json!({
            "[runtime secret redacted] 1": "[runtime secret redacted]",
            "[runtime secret redacted]": "keep",
            "nested": [{"echo": "Bearer [runtime secret redacted]"}]
        })
    );
}
