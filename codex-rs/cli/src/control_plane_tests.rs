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
            "target_status": {"agent/path": "running"},
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
    assert_eq!(events[0].observed_host_return.as_ref().unwrap().target_statuses.len(), 1);
    assert!(events[0].observed_host_return.as_ref().unwrap().complete);
    assert_eq!(coverage.malformed_records, 0);
    assert_eq!(coverage.unmatched_calls, 0);
}

#[test]
fn recognized_external_wait_flows_through_recorder_shared_reducer_and_cli_projection() {
    let input = rollout_pair(
        "wait_agent",
        serde_json::json!({"targets": ["agent/path"], "return_when": "target_terminal"}),
        serde_json::json!({"reason": "target_terminal", "target_status": {"agent/path": "running"}}),
    );
    let (observations, parser_coverage) = envelopes::parse(input.as_bytes(), "selected-host-session");
    assert_eq!(observations.len(), 1);
    assert_eq!(parser_coverage.unmatched_calls, 0);
    let recorder = codex_diagnostics::control_plane::ControlPlaneRecorder::new(
        codex_diagnostics::control_plane::CaptureMode::Session, "selected-capture").unwrap();
    let mut event_projections = Vec::new();
    for observation in observations {
        event_projections.push(super::project_envelope(&observation));
        assert!(recorder.record(super::to_event(observation).unwrap()).is_some());
    }
    let snapshot = recorder.snapshot().unwrap();
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].identity.capture_instance_id, "selected-capture");
    assert_eq!(snapshot.events[0].identity.sequence, 1);
    let reduced = codex_diagnostics::control_plane::Summary::reduce_snapshot(&snapshot);
    assert_eq!(reduced.wait_timelines.len(), 1);
    assert_eq!(reduced.wait_timelines[0].return_when,
        codex_diagnostics::control_plane::ReturnWhen::Unknown);
    assert_eq!(reduced.wait_timelines[0].operation_id, None);
    let rendered = summary::project(&reduced);
    let wait = &rendered["waitTimelines"][0];
    assert_eq!(wait["returnWhen"], "Unknown");
    assert_eq!(wait["requestedTargetReferences"][0], "agent/path");
    assert_eq!(wait["observedHostReturn"]["targetStatuses"][0]["statusTag"], "Running");
    let command = super::ControlPlaneCommand {
        input: std::path::PathBuf::new(),
        start_byte: 0,
        end_byte: 1,
        source_namespace: "selected-host-session".to_owned(),
        source_plane: super::SourcePlaneSelector::ExternalHostEnvelope,
        root_thread_id: None,
        usage_request: None,
    };
    let rendered = super::render(
        Some(&reduced), &parser_coverage, &super::Coverage::default(), Some(&snapshot),
        &command, event_projections, serde_json::json!({"requested": false}),
    );
    assert_eq!(rendered["schemaVersion"], 1);
    assert_eq!(rendered["coverage"]["unmatchedCalls"], 0);
    assert_eq!(rendered["recorder"]["captureInstanceId"], "selected-capture");
    assert_eq!(rendered["events"].as_array().unwrap().len(), 1);
    assert!(rendered["data"]["waitTimelines"][0]["observedHostReturn"].is_object());
}

#[test]
fn identical_replay_is_deduplicated_conflict_is_quarantined_and_open_call_counted() {
    let pair = rollout_pair(
        "wait_agent",
        serde_json::json!({"targets": ["agent/path"], "return_when": "any"}),
        serde_json::json!({"reason": "target_terminal", "target_status": {"agent/path": "running"}}),
    );
    let mut lines = pair.lines().map(str::to_owned).collect::<Vec<_>>();
    lines.push(lines[0].clone());
    lines.push(lines[1].replace("target_terminal", "timeout"));
    let (events, coverage) = envelopes::parse(lines.join("\n").as_bytes(), "explicit-session");
    assert!(events.is_empty());
    assert_eq!(coverage.duplicate_records, 2);
    assert_eq!(coverage.conflicting_pairs, 1);
    assert_eq!(coverage.unmatched_calls, 0);

    let open_call = serde_json::json!({
        "timestamp": "2026-10-05T01:00:00Z", "type": "response_item",
        "payload": {"type": "function_call", "name": "wait_agent", "call_id": "open-call",
            "arguments": "{\"targets\":[\"agent/path\"],\"return_when\":\"all\"}"}
    });
    let (events, coverage) = envelopes::parse(open_call.to_string().as_bytes(), "explicit-session");
    assert!(events.is_empty());
    assert_eq!(coverage.unmatched_calls, 1);
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
fn unsupported_host_status_tag_is_incomplete_and_cannot_claim_exact_result() {
    let input = rollout_pair(
        "list_agents",
        serde_json::json!({"path_prefix": "/agents"}),
        serde_json::json!({"agents": [
            {"agent_id": "actor-a", "canonical_path": "/agents/a", "agent_status": "paused"}
        ]}),
    );
    let (observations, coverage) = envelopes::parse(input.as_bytes(), "selected-host-session");
    assert_eq!(coverage.incomplete_status_results, 1);
    let result = observations[0].status_result.as_ref().unwrap();
    assert!(!result.complete);
    assert_eq!(result.actor_count, 1);
    assert!(result.actors.is_empty());
    let event = super::to_event(observations.into_iter().next().unwrap()).unwrap();
    let recorder = codex_diagnostics::control_plane::ControlPlaneRecorder::new(
        codex_diagnostics::control_plane::CaptureMode::Session, "selected-capture").unwrap();
    recorder.record(event).unwrap();
    assert_eq!(codex_diagnostics::control_plane::Summary::reduce_snapshot(
        &recorder.snapshot().unwrap()).repeated_status_query_groups, 0);
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
        "invalidEventIdentityCount",
    ] {
        assert!(object.contains_key(field), "missing summary field {field}");
    }
}
