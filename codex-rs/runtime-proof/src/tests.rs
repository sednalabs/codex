use crate::wire;
use pretty_assertions::assert_eq;
use serde_json::Value;

#[test]
fn canonical_claim_digest_uses_the_ops_jcs_contract() {
    let first = serde_json::json!({
        "claim_precondition": {
            "expected_generation": 0,
            "expected_updated_at": "2026-10-04T05:00:00Z",
            "request_id": "8f14e45f-ea42-4d73-9ca9-9a8b35d1c05a",
        },
        "note": "café",
        "work_item_ref": "example-work-item",
    });
    let reordered: Value = serde_json::from_str(
        r#"{"work_item_ref":"example-work-item","note":"café","claim_precondition":{"request_id":"8f14e45f-ea42-4d73-9ca9-9a8b35d1c05a","expected_updated_at":"2026-10-04T05:00:00Z","expected_generation":0}}"#,
    )
    .unwrap();
    let first = wire::canonical_operation("work_item_claim", &first).unwrap();
    let reordered = wire::canonical_operation("work_item_claim", &reordered).unwrap();
    assert_eq!(first, reordered);
    assert_eq!(
        String::from_utf8(first.clone()).unwrap(),
        r#"{"method":"tools/call","parameters":{"claim_precondition":{"expected_generation":0,"expected_updated_at":"2026-10-04T05:00:00Z","request_id":"8f14e45f-ea42-4d73-9ca9-9a8b35d1c05a"},"note":"café","work_item_ref":"example-work-item"},"tool":"work_item_claim","transport":"mcp"}"#
    );
    assert_eq!(
        wire::digest(&first),
        "sha256:f952a1f6a574c1f1cc92e3266f096def65395971ec629dce266daaa5f973d4f2"
    );
}

#[test]
fn canonical_claim_digest_distinguishes_omission_null_and_note_changes() {
    let omitted = serde_json::json!({ "work_item_ref": "example-work-item" });
    let null_note = serde_json::json!({ "work_item_ref": "example-work-item", "note": null });
    let changed_note =
        serde_json::json!({ "work_item_ref": "example-work-item", "note": "changed" });
    let omitted = wire::canonical_operation("work_item_claim", &omitted).unwrap();
    let null_note = wire::canonical_operation("work_item_claim", &null_note).unwrap();
    let changed_note = wire::canonical_operation("work_item_claim", &changed_note).unwrap();
    assert_ne!(omitted, null_note);
    assert_ne!(null_note, changed_note);
}

#[test]
fn canonical_claim_digest_uses_jcs_utf16_key_order() {
    let parameters = serde_json::json!({ "": 1, "𐀀": 2 });
    let canonical = wire::canonical_operation("work_item_claim", &parameters).unwrap();
    let canonical = String::from_utf8(canonical).unwrap();
    assert!(canonical.find("𐀀").unwrap() < canonical.find("").unwrap());
}

#[test]
fn canonical_claim_digest_rejects_unsafe_integers_and_floats() {
    let max_safe = serde_json::json!({ "generation": (1_u64 << 53) - 1 });
    let too_large = serde_json::json!({ "generation": 1_u64 << 53 });
    let too_small = serde_json::json!({ "generation": -(1_i64 << 53) });
    let float = serde_json::json!({ "generation": 1.5 });
    assert!(wire::canonical_operation("work_item_claim", &max_safe).is_ok());
    assert!(wire::canonical_operation("work_item_claim", &too_large).is_err());
    assert!(wire::canonical_operation("work_item_claim", &too_small).is_err());
    assert!(wire::canonical_operation("work_item_claim", &float).is_err());
}
