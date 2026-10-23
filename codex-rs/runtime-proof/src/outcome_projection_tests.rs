use super::MAX_LINE_BYTES;
use super::project_child_stdout;
use serde_json::Value;
use uuid::Uuid;

const THREAD: &str = "11111111-1111-4111-8111-111111111111";
const PRINCIPAL: &str = "22222222-2222-4222-8222-222222222222";
const INSTANCE: &str = "33333333-3333-4333-8333-333333333333";
const REQUEST: &str = "44444444-4444-4444-8444-444444444444";

fn line(value: Value, output: &mut Vec<u8>) {
    serde_json::to_writer(&mut *output, &value).unwrap();
    output.push(b'\n');
}

fn item_event(kind: &str, item: Value) -> Value {
    serde_json::json!({
        "type": kind,
        "thread_id": THREAD,
        "turn_id": "turn-1",
        "item": item,
    })
}

#[test]
fn primary_claim_and_current_snapshot_are_projected_without_result_text() {
    let mut input = Vec::new();
    line(
        serde_json::json!({"type":"thread.started","thread_id":THREAD}),
        &mut input,
    );
    line(
        item_event(
            "item.started",
            serde_json::json!({
                "id":"item_0",
                "type":"mcp_tool_call",
                "server":"ops",
                "tool":"work_item_claim",
                "arguments":{
                    "work_item_ref":"w18773",
                    "claim_precondition":{
                        "expected_generation":0,
                        "expected_updated_at":"2026-10-23T14:00:00Z",
                        "request_id":REQUEST
                    },
                    "proof":"private-proof"
                }
            }),
        ),
        &mut input,
    );
    line(
        item_event(
            "item.completed",
            serde_json::json!({
                "id":"item_0",
                "type":"mcp_tool_call",
                "server":"ops",
                "tool":"work_item_claim",
                "arguments":{},
                "status":"completed",
                "result":{"content":[{"text":"private-result-text"}],"structured_content":{
                    "work_item_ref":"w18773"
                }}
            }),
        ),
        &mut input,
    );
    line(
        item_event(
            "item.started",
            serde_json::json!({
                "id":"item_1",
                "type":"mcp_tool_call",
                "server":"ops",
                "tool":"work_item_current_work",
                "arguments":{"work_item_ref":"w18773"}
            }),
        ),
        &mut input,
    );
    line(
        item_event(
            "item.completed",
            serde_json::json!({
                "id":"item_1",
                "type":"mcp_tool_call",
                "server":"ops",
                "tool":"work_item_current_work",
                "arguments":{},
                "status":"completed",
                "result":{"structured_content":{
                    "work_item_ref":"w18773",
                    "work_revision":{"updated_at":"2026-10-23T14:01:00Z"},
                    "current_claim":{
                        "generation":1,
                        "execution_instance_id":INSTANCE,
                        "principal_id":PRINCIPAL,
                        "released":false
                    },
                    "secret":"do-not-project"
                }}
            }),
        ),
        &mut input,
    );

    let mut output = Vec::new();
    project_child_stdout(
        input.as_slice(),
        "w18773",
        Uuid::parse_str(PRINCIPAL).unwrap(),
        &mut output,
    );
    let text = String::from_utf8(output.clone()).unwrap();
    assert!(!text.contains("private-proof"));
    assert!(!text.contains("private-result-text"));
    assert!(!text.contains("do-not-project"));
    let records: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    assert_eq!(records[0]["request_id"], REQUEST);
    assert_eq!(records[0]["outcome"], "unknown");
    assert_eq!(records[1]["outcome"], "claimed");
    assert_eq!(records[2]["current_claim"]["generation"], 1);
    assert_eq!(
        records[2]["current_claim"]["execution_instance_id"],
        INSTANCE
    );
    assert_eq!(records[2]["current_claim"]["principal_matches"], true);
}

#[test]
fn exact_holder_conflict_is_classified_but_other_errors_remain_unknown() {
    let mut input = Vec::new();
    line(
        serde_json::json!({"type":"thread.started","thread_id":THREAD}),
        &mut input,
    );
    for (id, error) in [
        (
            "item_0",
            "work_item_execution.holder_conflict: another active execution holds this work item.",
        ),
        ("item_1", "private database error and secret text"),
    ] {
        line(
            item_event(
                "item.started",
                serde_json::json!({
                    "id":id,
                    "type":"mcp_tool_call",
                    "server":"ops",
                    "tool":"work_item_claim",
                    "arguments":{
                        "work_item_ref":"w18773",
                        "claim_precondition":{
                            "expected_generation":1,
                            "expected_updated_at":"2026-10-23T14:01:00Z",
                            "request_id":if id == "item_0" { REQUEST } else { INSTANCE }
                        }
                    }
                }),
            ),
            &mut input,
        );
        line(
            item_event(
                "item.completed",
                serde_json::json!({
                    "id":id,
                    "type":"mcp_tool_call",
                    "server":"ops",
                    "tool":"work_item_claim",
                    "arguments":{},
                    "status":"failed",
                    "error":{"message":error}
                }),
            ),
            &mut input,
        );
    }
    let mut output = Vec::new();
    project_child_stdout(
        input.as_slice(),
        "w18773",
        Uuid::parse_str(PRINCIPAL).unwrap(),
        &mut output,
    );
    let text = String::from_utf8(output.clone()).unwrap();
    assert!(!text.contains("private database error"));
    let records: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(records[1]["outcome"], "holder_conflict");
    assert_eq!(records[3]["outcome"], "unknown");
}

#[test]
fn release_result_projects_only_its_compact_receipt_fields() {
    let mut input = Vec::new();
    line(
        serde_json::json!({"type":"thread.started","thread_id":THREAD}),
        &mut input,
    );
    line(
        item_event(
            "item.started",
            serde_json::json!({
                "id":"item_0",
                "type":"mcp_tool_call",
                "server":"ops",
                "tool":"work_item_release",
                "arguments":{
                    "work_item_ref":"w18773",
                    "claim_precondition":{
                        "expected_generation":2,
                        "expected_updated_at":"2026-10-23T14:05:00Z",
                        "request_id":REQUEST
                    }
                }
            }),
        ),
        &mut input,
    );
    line(
        item_event(
            "item.completed",
            serde_json::json!({
                "id":"item_0",
                "type":"mcp_tool_call",
                "server":"ops",
                "tool":"work_item_release",
                "arguments":{},
                "status":"completed",
                "result":{"structured_content":{
                    "work_item_ref":"w18773",
                    "request_id":REQUEST,
                    "outcome":"released",
                    "generation":2,
                    "execution_instance_id":INSTANCE,
                    "released":true,
                    "historical":{"private":"not forwarded"}
                }}
            }),
        ),
        &mut input,
    );
    let mut output = Vec::new();
    project_child_stdout(
        input.as_slice(),
        "w18773",
        Uuid::parse_str(PRINCIPAL).unwrap(),
        &mut output,
    );
    let text = String::from_utf8(output.clone()).unwrap();
    assert!(!text.contains("private"));
    let records: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(records[1]["outcome"], "released");
    assert_eq!(records[1]["generation"], 2);
    assert_eq!(records[1]["execution_instance_id"], INSTANCE);
    assert_eq!(records[1]["released"], true);
    assert_eq!(records[1]["historical_present"], true);
}

#[test]
fn release_generation_must_equal_the_claim_precondition() {
    let mut input = Vec::new();
    line(
        serde_json::json!({"type":"thread.started","thread_id":THREAD}),
        &mut input,
    );
    line(
        item_event(
            "item.started",
            serde_json::json!({
                "id":"item_0",
                "type":"mcp_tool_call",
                "server":"ops",
                "tool":"work_item_release",
                "arguments":{
                    "work_item_ref":"w18773",
                    "claim_precondition":{
                        "expected_generation":2,
                        "expected_updated_at":"2026-10-23T14:05:00Z",
                        "request_id":REQUEST
                    }
                }
            }),
        ),
        &mut input,
    );
    line(
        item_event(
            "item.completed",
            serde_json::json!({
                "id":"item_0",
                "type":"mcp_tool_call",
                "server":"ops",
                "tool":"work_item_release",
                "arguments":{},
                "status":"completed",
                "result":{"structured_content":{
                    "work_item_ref":"w18773",
                    "request_id":REQUEST,
                    "outcome":"released",
                    "generation":3,
                    "execution_instance_id":INSTANCE,
                    "released":true
                }}
            }),
        ),
        &mut input,
    );
    let mut output = Vec::new();
    project_child_stdout(
        input.as_slice(),
        "w18773",
        Uuid::parse_str(PRINCIPAL).unwrap(),
        &mut output,
    );
    let records: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(records[1]["outcome"], "unresolved");
}

#[test]
fn exact_receipt_recovery_is_correlated_and_null_stays_unresolved() {
    let mut input = Vec::new();
    line(
        serde_json::json!({"type":"thread.started","thread_id":THREAD}),
        &mut input,
    );
    for (id, response) in [
        (
            "item_0",
            serde_json::json!({
                "work_item_ref":"w18773",
                "request_id":REQUEST,
                "historical":{
                    "outcome":"claimed",
                    "generation":4,
                    "execution_instance_id":INSTANCE
                },
                "current_claim":{"private":"not forwarded"}
            }),
        ),
        (
            "item_1",
            serde_json::json!({
                "work_item_ref":"w18773",
                "request_id":REQUEST,
                "historical":null
            }),
        ),
    ] {
        line(
            item_event(
                "item.started",
                serde_json::json!({
                    "id":id,
                    "type":"mcp_tool_call",
                    "server":"ops",
                    "tool":"work_item_execution_receipt_read",
                    "arguments":{"work_item_ref":"w18773","request_id":REQUEST}
                }),
            ),
            &mut input,
        );
        line(
            item_event(
                "item.completed",
                serde_json::json!({
                    "id":id,
                    "type":"mcp_tool_call",
                    "server":"ops",
                    "tool":"work_item_execution_receipt_read",
                    "arguments":{},
                    "status":"completed",
                    "result":{"structured_content":response}
                }),
            ),
            &mut input,
        );
    }
    let mut output = Vec::new();
    project_child_stdout(
        input.as_slice(),
        "w18773",
        Uuid::parse_str(PRINCIPAL).unwrap(),
        &mut output,
    );
    let text = String::from_utf8(output.clone()).unwrap();
    assert!(!text.contains("not forwarded"));
    let records: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(records[0]["outcome"], "receipt_found");
    assert_eq!(records[0]["request_id"], REQUEST);
    assert_eq!(records[0]["historical"]["generation"], 4);
    assert_eq!(records[1]["outcome"], "unresolved");
    assert_eq!(records[1]["request_id"], REQUEST);
}

#[test]
fn current_work_requires_the_exact_ref_and_explicit_claim_state() {
    let mut input = Vec::new();
    line(
        serde_json::json!({"type":"thread.started","thread_id":THREAD}),
        &mut input,
    );
    for (id, result) in [
        (
            "item_0",
            serde_json::json!({
                "work_item_ref":"w99999",
                "work_revision":{"updated_at":"2026-10-23T14:01:00Z"},
                "current_claim":null
            }),
        ),
        (
            "item_1",
            serde_json::json!({
                "work_item_ref":"w18773",
                "work_revision":{"updated_at":"2026-10-23T14:01:00Z"}
            }),
        ),
        (
            "item_2",
            serde_json::json!({
                "work_item_ref":"w18773",
                "work_revision":{"updated_at":"2026-10-23T14:01:00Z"},
                "current_claim":null
            }),
        ),
    ] {
        line(
            item_event(
                "item.started",
                serde_json::json!({
                    "id":id,
                    "type":"mcp_tool_call",
                    "server":"ops",
                    "tool":"work_item_current_work",
                    "arguments":{"work_item_ref":"w18773"}
                }),
            ),
            &mut input,
        );
        line(
            item_event(
                "item.completed",
                serde_json::json!({
                    "id":id,
                    "type":"mcp_tool_call",
                    "server":"ops",
                    "tool":"work_item_current_work",
                    "arguments":{},
                    "status":"completed",
                    "result":{"structured_content":result}
                }),
            ),
            &mut input,
        );
    }
    let mut output = Vec::new();
    project_child_stdout(
        input.as_slice(),
        "w18773",
        Uuid::parse_str(PRINCIPAL).unwrap(),
        &mut output,
    );
    let records: Vec<Value> = output
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    assert_eq!(records[0]["outcome"], "unresolved");
    assert_eq!(records[1]["outcome"], "unresolved");
    assert_eq!(records[2]["outcome"], "read");
    assert_eq!(records[2]["expected_generation"], 0);
}

#[test]
fn child_events_are_ignored_and_overlong_lines_are_drained() {
    let mut input = vec![b'x'; MAX_LINE_BYTES + 8];
    input.push(b'\n');
    line(
        serde_json::json!({"type":"thread.started","thread_id":THREAD}),
        &mut input,
    );
    line(
        serde_json::json!({
            "type":"item.started",
            "thread_id":"55555555-5555-4555-8555-555555555555",
            "item":{
                "id":"child-item",
                "type":"mcp_tool_call",
                "server":"ops",
                "tool":"work_item_claim",
                "arguments":{
                    "work_item_ref":"w18773",
                    "claim_precondition":{
                        "expected_generation":0,
                        "expected_updated_at":"2026-10-23T14:00:00Z",
                        "request_id":REQUEST
                    }
                }
            }
        }),
        &mut input,
    );
    let mut output = Vec::new();
    project_child_stdout(
        input.as_slice(),
        "w18773",
        Uuid::parse_str(PRINCIPAL).unwrap(),
        &mut output,
    );
    assert!(output.is_empty());
}
