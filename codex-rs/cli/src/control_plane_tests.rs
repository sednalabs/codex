use super::envelopes;
use super::summary;

fn rollout_pair(name: &str, arguments: serde_json::Value, output: serde_json::Value) -> String {
    let call = serde_json::json!({
        "timestamp": "2026-10-05T01:00:00Z",
        "type": "response_item",
        "payload": {
            "type": "function_call",
            "name": name,
            "call_id": "selected-call",
            "arguments": arguments.to_string()
        }
    });
    let result = serde_json::json!({
        "timestamp": "2026-10-05T01:00:02Z",
        "type": "response_item",
        "payload": {
            "type": "function_call_output",
            "call_id": "selected-call",
            "output": output.to_string()
        }
    });
    format!("{call}\n{result}")
}

#[test]
fn wait_call_output_pair_uses_exact_id_and_discards_body() {
    let input = rollout_pair(
        "wait_agent",
        serde_json::json!({
            "targets": ["agent/path"],
            "return_when": "target_terminal",
            "timeout_ms": 30000
        }),
        serde_json::json!({
            "timed_out": false,
            "reason": "target_terminal",
            "wake_cause": "target_status",
            "queued_update_count": 3,
            "target_status": {"private": "ignored"},
            "completed": {"privateBody": "ignored"}
        }),
    );
    let (events, coverage) = envelopes::parse(input.as_bytes(), "explicit-session");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].call_id, "selected-call");
    assert_eq!(
        events[0].wait_request.as_ref().unwrap().targets,
        vec!["agent/path".to_owned()]
    );
    assert_eq!(events[0].host_target_terminal, Some(true));
    assert_eq!(events[0].host_target_status_wake, Some(true));
    assert_eq!(coverage.malformed_records, 0);
}

#[test]
fn list_agents_keeps_only_bounded_typed_actor_metadata() {
    let input = rollout_pair(
        "list_agents",
        serde_json::json!({"path_prefix": "/agents"}),
        serde_json::json!({"agents": [
            {"agent_id": "actor-a", "canonical_path": "/agents/a", "configured_model": "gpt-luna", "configured_reasoning_effort": "medium", "agent_status": "running"},
            {"agent_id": "actor-b", "canonical_path": "/agents/b", "agent_status": {"completed": {"privateBody": "ignored"}}}
        ]}),
    );
    let (events, coverage) = envelopes::parse(input.as_bytes(), "explicit-session");
    let result = events[0].status_result.as_ref().unwrap();
    assert!(result.complete);
    assert_eq!(result.actor_count, 2);
    assert_eq!(result.actors[0].status, envelopes::ExternalStatusTag::Running);
    assert_eq!(result.actors[1].status, envelopes::ExternalStatusTag::Completed);
    assert_eq!(coverage.malformed_records, 0);
}

#[test]
fn malformed_and_oversized_records_have_content_free_coverage() {
    let mut input = vec![b'x'; 65_537];
    input.push(b'\n');
    input.extend_from_slice(b"{\"private\":\"not echoed\"}");
    let (events, coverage) = envelopes::parse(&input, "explicit-session");
    assert!(events.is_empty());
    assert_eq!(coverage.oversized_records, 1);
    assert_eq!(coverage.malformed_records, 1);
}

#[test]
fn common_summary_serialization_includes_all_timeline_planes() {
    let projected = summary::project(&codex_diagnostics::control_plane::Summary::default());
    let object = projected.as_object().unwrap();
    for field in [
        "waitTimelines",
        "sleepTimelines",
        "schedulerTimelines",
        "messageTimelines",
        "boundaryTimelines",
        "providerCallTimelines",
        "queueTimelines",
        "externalDurations",
        "repeatedWaitGroups",
        "repeatedStatusQueryGroups",
        "partitionsConserve",
    ] {
        assert!(object.contains_key(field), "missing summary field {field}");
    }
}
